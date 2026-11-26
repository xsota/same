//! Quota Adapter: asks each agent for the usage limits of the logged-in
//! account, without running a turn.
//!
//! - Claude Code: the stream-json `get_usage` control request (with
//!   `skip_behaviors`, so the CLI does not scan local transcripts).
//! - Codex: `account/rateLimits/read` on `codex app-server`.
//! - ACP agents: no standard way to ask, so always unavailable.
//!
//! Only what the provider reports is returned; nothing is estimated.

use crate::discovery::json_lines_exchange;
use anyhow::{anyhow, Result};
use chrono::{DateTime, TimeZone, Utc};
use same_acp::exec::ExecTarget;
use same_core::{
    ProviderConfig, ProviderKind, QuotaReport, QuotaSnapshot, QuotaWindow, QuotaWindowKind,
};
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;

const QUOTA_TIMEOUT: Duration = Duration::from_secs(30);

const FIVE_HOURS: u32 = 5 * 60;
const SEVEN_DAYS: u32 = 7 * 24 * 60;

/// Returns `provider`'s quota when run in `target`. An `Err` is a failure to
/// ask (the agent did not start or answer); a provider that cannot report
/// its quota gives [`QuotaReport::Unavailable`].
pub async fn fetch_quota(
    provider: &ProviderConfig,
    target: &ExecTarget,
    cwd: &Path,
) -> Result<QuotaReport> {
    let fut = async {
        match provider.kind {
            ProviderKind::Acp => Ok(QuotaReport::Unavailable(
                "ACP agents do not report usage limits".to_string(),
            )),
            ProviderKind::ClaudeCli => fetch_claude(provider, target, cwd).await,
            ProviderKind::CodexCli => fetch_codex(provider, target, cwd).await,
        }
    };
    tokio::time::timeout(QUOTA_TIMEOUT, fut)
        .await
        .map_err(|_| anyhow!("timed out"))?
}

async fn fetch_claude(
    provider: &ProviderConfig,
    target: &ExecTarget,
    cwd: &Path,
) -> Result<QuotaReport> {
    const REQ_ID: &str = "same-usage";
    json_lines_exchange(
        provider,
        target,
        cwd,
        &[
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
        ],
        &[
            json!({
                "type": "control_request",
                "request_id": "same-init",
                "request": { "subtype": "initialize" }
            }),
            json!({
                "type": "control_request",
                "request_id": REQ_ID,
                "request": { "subtype": "get_usage", "skip_behaviors": true }
            }),
        ],
        |val| {
            let resp = val.get("response")?;
            if val.get("type")?.as_str()? != "control_response"
                || resp.get("request_id")?.as_str()? != REQ_ID
            {
                return None;
            }
            Some(Ok(parse_claude_usage(resp, Utc::now())))
        },
    )
    .await
}

/// Parses the `response` object of a `get_usage` control response.
fn parse_claude_usage(resp: &Value, now: DateTime<Utc>) -> QuotaReport {
    if resp.get("subtype").and_then(Value::as_str) == Some("error") {
        // Older CLIs don't know `get_usage`; some contexts refuse it.
        let why = resp
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("get_usage is not supported");
        return QuotaReport::Unavailable(why.to_string());
    }
    let Some(usage) = resp.get("response") else {
        return QuotaReport::Unavailable("empty get_usage response".to_string());
    };
    if usage.get("rate_limits_available").and_then(Value::as_bool) != Some(true) {
        return QuotaReport::Unavailable(
            "No plan limits for this login (API key or unsupported plan)".to_string(),
        );
    }
    let limits = usage.get("rate_limits");
    let window = |key: &str, kind: QuotaWindowKind| {
        let w = limits?.get(key)?;
        Some(QuotaWindow {
            kind,
            used_percent: w.get("utilization")?.as_f64()?,
            resets_at: w
                .get("resets_at")
                .and_then(Value::as_str)
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.with_timezone(&Utc)),
        })
    };
    let model = |model: &str| QuotaWindowKind::Model {
        minutes: SEVEN_DAYS,
        model: model.to_string(),
    };
    let windows = [
        window(
            "five_hour",
            QuotaWindowKind::Rolling {
                minutes: FIVE_HOURS,
            },
        ),
        window(
            "seven_day",
            QuotaWindowKind::Rolling {
                minutes: SEVEN_DAYS,
            },
        ),
        window("seven_day_opus", model("Opus")),
        window("seven_day_sonnet", model("Sonnet")),
    ]
    .into_iter()
    .flatten()
    .collect();
    QuotaReport::Available(QuotaSnapshot {
        plan: usage
            .get("subscription_type")
            .and_then(Value::as_str)
            .map(str::to_string),
        windows,
        fetched_at: now,
    })
}

async fn fetch_codex(
    provider: &ProviderConfig,
    target: &ExecTarget,
    cwd: &Path,
) -> Result<QuotaReport> {
    json_lines_exchange(
        provider,
        target,
        cwd,
        &["app-server"],
        &[
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                   "params": {"clientInfo": {"name": "same", "version": env!("CARGO_PKG_VERSION")}}}),
            json!({"jsonrpc": "2.0", "method": "initialized"}),
            json!({"jsonrpc": "2.0", "id": 2, "method": "account/rateLimits/read", "params": null}),
        ],
        |val| {
            if val.get("id")?.as_i64()? != 2 {
                return None;
            }
            Some(Ok(parse_codex_rate_limits(val, Utc::now())))
        },
    )
    .await
}

/// Parses the JSON-RPC reply to `account/rateLimits/read`.
fn parse_codex_rate_limits(val: &Value, now: DateTime<Utc>) -> QuotaReport {
    if let Some(err) = val.get("error") {
        // E.g. API-key auth, which has no plan limits.
        let why = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("account/rateLimits/read failed");
        return QuotaReport::Unavailable(why.to_string());
    }
    let Some(main) = val.pointer("/result/rateLimits").filter(|v| v.is_object()) else {
        return QuotaReport::Unavailable("No rate limits for this login".to_string());
    };
    let main_id = main.get("limitId").and_then(Value::as_str);

    let mut windows = Vec::new();
    let mut push = |limit: &Value, model: Option<&str>| {
        for key in ["primary", "secondary"] {
            let Some(w) = limit.get(key).filter(|w| w.is_object()) else {
                continue;
            };
            let Some(used) = w.get("usedPercent").and_then(Value::as_f64) else {
                continue;
            };
            let minutes = w
                .get("windowDurationMins")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32;
            windows.push(QuotaWindow {
                kind: match model {
                    Some(m) => QuotaWindowKind::Model {
                        minutes,
                        model: m.to_string(),
                    },
                    None => QuotaWindowKind::Rolling { minutes },
                },
                used_percent: used,
                resets_at: w
                    .get("resetsAt")
                    .and_then(Value::as_i64)
                    .and_then(|s| Utc.timestamp_opt(s, 0).single()),
            });
        }
    };
    push(main, None);
    // Separate limits (e.g. for one model) besides the account's main one.
    if let Some(by_id) = val
        .pointer("/result/rateLimitsByLimitId")
        .and_then(Value::as_object)
    {
        for (id, limit) in by_id {
            if Some(id.as_str()) == main_id {
                continue;
            }
            let name = ["limitName", "normalModelSlug"]
                .iter()
                .find_map(|k| limit.get(*k).and_then(Value::as_str))
                .unwrap_or(id);
            push(limit, Some(name));
        }
    }
    QuotaReport::Available(QuotaSnapshot {
        plan: main
            .get("planType")
            .and_then(Value::as_str)
            .map(str::to_string),
        windows,
        fetched_at: now,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
    }

    #[test]
    fn claude_usage_reads_plan_windows() {
        let resp = json!({
            "subtype": "success",
            "request_id": "same-usage",
            "response": {
                "subscription_type": "pro",
                "rate_limits_available": true,
                "rate_limits": {
                    "five_hour": {"utilization": 10, "resets_at": "2026-01-01T05:00:00.5+00:00"},
                    "seven_day": {"utilization": 54.5, "resets_at": "2026-01-04T00:00:00+00:00"},
                    "seven_day_opus": null,
                    "seven_day_sonnet": {"utilization": 3, "resets_at": null},
                    "some_codename": {"utilization": 99}
                },
                "behaviors": null
            }
        });
        let QuotaReport::Available(snap) = parse_claude_usage(&resp, now()) else {
            panic!("expected a snapshot");
        };
        assert_eq!(snap.plan.as_deref(), Some("pro"));
        assert_eq!(snap.fetched_at, now());
        assert_eq!(snap.windows.len(), 3);
        assert_eq!(
            snap.windows[0].kind,
            QuotaWindowKind::Rolling {
                minutes: FIVE_HOURS
            }
        );
        assert_eq!(snap.windows[0].used_percent, 10.0);
        assert_eq!(
            snap.windows[0].resets_at.map(|d| d.timestamp()),
            Some(now().timestamp() + 5 * 3600)
        );
        assert_eq!(snap.windows[1].used_percent, 54.5);
        assert_eq!(
            snap.windows[2].kind,
            QuotaWindowKind::Model {
                minutes: SEVEN_DAYS,
                model: "Sonnet".into()
            }
        );
        assert_eq!(snap.windows[2].resets_at, None);
    }

    #[test]
    fn claude_usage_unavailable_cases() {
        let api_key = json!({"subtype": "success", "response": {"rate_limits_available": false}});
        assert!(matches!(
            parse_claude_usage(&api_key, now()),
            QuotaReport::Unavailable(_)
        ));
        let old_cli =
            json!({"subtype": "error", "error": "Unsupported control request subtype: get_usage"});
        assert_eq!(
            parse_claude_usage(&old_cli, now()),
            QuotaReport::Unavailable("Unsupported control request subtype: get_usage".into())
        );
    }

    #[test]
    fn codex_rate_limits_read_windows_and_plan() {
        let val = json!({"id": 2, "result": {
            "rateLimits": {
                "limitId": "codex", "limitName": null,
                "primary": {"usedPercent": 5, "windowDurationMins": 300, "resetsAt": 1767243600},
                "secondary": {"usedPercent": 13, "windowDurationMins": 10080, "resetsAt": 1767830400},
                "planType": "plus"
            },
            "rateLimitsByLimitId": {
                "codex": {"limitId": "codex", "primary": {"usedPercent": 5, "windowDurationMins": 300}},
                "codex_other": {"limitId": "codex_other", "limitName": "GPT-Spark",
                    "primary": {"usedPercent": 40, "windowDurationMins": 300, "resetsAt": null},
                    "secondary": null}
            }
        }});
        let QuotaReport::Available(snap) = parse_codex_rate_limits(&val, now()) else {
            panic!("expected a snapshot");
        };
        assert_eq!(snap.plan.as_deref(), Some("plus"));
        assert_eq!(snap.windows.len(), 3);
        assert_eq!(
            snap.windows[0].kind,
            QuotaWindowKind::Rolling { minutes: 300 }
        );
        assert_eq!(snap.windows[0].used_percent, 5.0);
        assert_eq!(
            snap.windows[0].resets_at.map(|d| d.timestamp()),
            Some(1767243600)
        );
        assert_eq!(
            snap.windows[1].kind,
            QuotaWindowKind::Rolling { minutes: 10080 }
        );
        assert_eq!(
            snap.windows[2].kind,
            QuotaWindowKind::Model {
                minutes: 300,
                model: "GPT-Spark".into()
            }
        );
        assert_eq!(snap.windows[2].resets_at, None);
    }

    #[test]
    fn codex_rate_limits_unavailable_cases() {
        let err =
            json!({"id": 2, "error": {"code": -32600, "message": "not logged in with ChatGPT"}});
        assert_eq!(
            parse_codex_rate_limits(&err, now()),
            QuotaReport::Unavailable("not logged in with ChatGPT".into())
        );
        let none = json!({"id": 2, "result": {"rateLimits": null}});
        assert!(matches!(
            parse_codex_rate_limits(&none, now()),
            QuotaReport::Unavailable(_)
        ));
        let empty =
            json!({"id": 2, "result": {"rateLimits": {"primary": null, "secondary": null}}});
        let QuotaReport::Available(snap) = parse_codex_rate_limits(&empty, now()) else {
            panic!("expected a snapshot");
        };
        assert!(snap.windows.is_empty());
    }
}
