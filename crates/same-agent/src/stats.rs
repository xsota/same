//! Token statistics: how many tokens each provider used over the last days,
//! read from the history the provider keeps on this machine.
//!
//! - Claude Code: `~/.claude/projects/**/*.jsonl` (or `$CLAUDE_CONFIG_DIR`),
//!   the `usage` of each assistant message, deduplicated by message and
//!   request id (one reply is written once per content block, and resumed
//!   sessions copy earlier messages).
//! - Codex: `~/.codex/sessions/**/*.jsonl` (or `$CODEX_HOME`): the per-response
//!   `token_usage_record` lines (deduplicated by response id), or, in older
//!   files without them, the growth of `token_count` totals.
//! - OpenCode: its own `opencode stats --json`, which also reports a cost.
//!
//! Everything else is unavailable. Only files are read; no service is asked.

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Days, Local, NaiveDate, TimeZone, Utc};
use same_acp::exec::{build_command, ExecTarget};
use same_core::{ProviderConfig, ProviderKind, StatsReport, TokenCounts, TokenStats};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime};

const STATS_TIMEOUT: Duration = Duration::from_secs(120);

/// Where a provider's token history comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatsSource {
    Claude,
    Codex,
    OpenCode,
}

/// The history `provider` keeps, if SAME can read it.
pub fn stats_source(provider: &ProviderConfig) -> Option<StatsSource> {
    match provider.kind {
        ProviderKind::ClaudeCli => Some(StatsSource::Claude),
        ProviderKind::CodexCli => Some(StatsSource::Codex),
        ProviderKind::Acp => {
            let name = Path::new(&provider.command)
                .file_stem()
                .map(|s| s.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            match name.as_str() {
                "opencode" => Some(StatsSource::OpenCode),
                "claude-agent-acp" | "claude-code-acp" => Some(StatsSource::Claude),
                "codex-acp" => Some(StatsSource::Codex),
                _ => None,
            }
        }
    }
}

/// The last `days` local days, today included: (first day, today).
fn period(days: u32) -> (NaiveDate, NaiveDate) {
    let today = Local::now().date_naive();
    let first = today
        .checked_sub_days(Days::new(u64::from(days.max(1) - 1)))
        .unwrap_or(today);
    (first, today)
}

/// Tokens `provider` used in the last `days` days (today included) when
/// run in `target`.
pub async fn fetch_stats(
    provider: &ProviderConfig,
    target: &ExecTarget,
    cwd: &Path,
    days: u32,
) -> Result<StatsReport> {
    let Some(source) = stats_source(provider) else {
        return Ok(StatsReport::Unavailable(
            "This agent keeps no token history SAME can read".to_string(),
        ));
    };
    let (first, today) = period(days);
    let fut = async {
        match source {
            StatsSource::Claude => {
                let root = data_dir(provider, target, cwd, "CLAUDE_CONFIG_DIR", ".claude").await?;
                scan_blocking(move || scan_claude(&root.join("projects"), first, today)).await
            }
            StatsSource::Codex => {
                let root = data_dir(provider, target, cwd, "CODEX_HOME", ".codex").await?;
                scan_blocking(move || scan_codex(&root, first, today)).await
            }
            StatsSource::OpenCode => opencode_stats(provider, target, cwd, first, today).await,
        }
    };
    tokio::time::timeout(STATS_TIMEOUT, fut)
        .await
        .map_err(|_| anyhow!("timed out"))?
}

async fn scan_blocking(
    scan: impl FnOnce() -> Result<Option<TokenStats>> + Send + 'static,
) -> Result<StatsReport> {
    let res = tokio::task::spawn_blocking(scan).await??;
    Ok(match res {
        Some(stats) => StatsReport::Available(stats),
        None => StatsReport::Unavailable("No local history found".to_string()),
    })
}

/// The provider's data directory (`$<env_var>`, else `~/<default>`), as a
/// path this process can read (a `\\wsl.localhost` path for WSL).
async fn data_dir(
    provider: &ProviderConfig,
    target: &ExecTarget,
    cwd: &Path,
    env_var: &str,
    default: &str,
) -> Result<PathBuf> {
    match target {
        ExecTarget::Host => {
            if let Some(dir) = provider
                .env
                .get(env_var)
                .cloned()
                .or_else(|| std::env::var(env_var).ok())
                .filter(|d| !d.is_empty())
            {
                let raw = if env_var == "CLAUDE_CONFIG_DIR" {
                    first_dir(&dir)
                } else {
                    dir.trim()
                };
                let p = PathBuf::from(raw);
                return Ok(if p.is_absolute() { p } else { cwd.join(p) });
            }
            let home = provider
                .env
                .get("HOME")
                .or_else(|| provider.env.get("USERPROFILE"))
                .filter(|h| !h.is_empty())
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME")
                        .or_else(|| std::env::var_os("USERPROFILE"))
                        .map(PathBuf::from)
                })
                .context("no home directory")?;
            Ok(home.join(default))
        }
        ExecTarget::Wsl { .. } => {
            let script = format!(r#"printf '%s\n%s' "$HOME" "${{{env_var}:-}}""#);
            let mut cmd = build_command(
                target,
                "sh",
                &["-c".to_string(), script],
                cwd,
                &provider.env,
            )
            .await;
            cmd.stdin(Stdio::null()).stderr(Stdio::null());
            let out = cmd.output().await.context("failed to start WSL")?;
            let text = String::from_utf8_lossy(&out.stdout);
            let mut lines = text.lines();
            let home = lines.next().unwrap_or("").trim().to_string();
            let custom = lines.next().unwrap_or("").trim().to_string();
            let linux = if !custom.is_empty() {
                if env_var == "CLAUDE_CONFIG_DIR" {
                    first_dir(&custom).to_string()
                } else {
                    custom
                }
            } else if home.starts_with('/') {
                format!("{}/{default}", home.trim_end_matches('/'))
            } else {
                bail!("no home directory in WSL");
            };
            Ok(target.to_host_path(cwd, &linux))
        }
    }
}

/// `CLAUDE_CONFIG_DIR` may list several directories; the first is used.
fn first_dir(value: &str) -> &str {
    value.split(',').next().unwrap_or(value).trim()
}

/// Running totals while scanning.
#[derive(Default)]
struct Tally {
    days: BTreeMap<NaiveDate, TokenCounts>,
    models: HashMap<String, TokenCounts>,
}

impl Tally {
    fn add(&mut self, day: NaiveDate, model: &str, counts: &TokenCounts) {
        if counts.is_zero() {
            return;
        }
        self.days.entry(day).or_default().add(counts);
        let model = if model.is_empty() { "unknown" } else { model };
        self.models
            .entry(model.to_string())
            .or_default()
            .add(counts);
    }

    fn finish(self, first: NaiveDate, today: NaiveDate, cost_usd: Option<f64>) -> TokenStats {
        let mut days = Vec::new();
        let mut total = TokenCounts::default();
        let mut day = first;
        while day <= today {
            let counts = self.days.get(&day).copied().unwrap_or_default();
            total.add(&counts);
            days.push((day, counts));
            day = match day.succ_opt() {
                Some(d) => d,
                None => break,
            };
        }
        let mut models: Vec<(String, TokenCounts)> = self.models.into_iter().collect();
        models.sort_by(|a, b| b.1.total().cmp(&a.1.total()).then(a.0.cmp(&b.0)));
        TokenStats {
            days,
            models,
            total,
            cost_usd,
            fetched_at: Utc::now(),
        }
    }
}

/// `*.jsonl` files under `root` changed since `since` (older files cannot
/// hold entries of the period). Returns an error if `root` itself cannot be read.
fn jsonl_files(root: &Path, since: SystemTime) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(err) if dir == root => {
                return Err(err).with_context(|| format!("failed to read {}", dir.display()));
            }
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "jsonl")
                && meta.modified().map_or(true, |m| m >= since)
            {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

fn local_midnight(day: NaiveDate) -> SystemTime {
    day.and_hms_opt(0, 0, 0)
        .and_then(|dt| Local.from_local_datetime(&dt).earliest())
        .map(SystemTime::from)
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// The local date of an RFC 3339 `timestamp` field, if it is in the period.
fn day_of(val: &Value, first: NaiveDate, today: NaiveDate) -> Option<NaiveDate> {
    let ts = val.get("timestamp")?.as_str()?;
    let day = DateTime::parse_from_rfc3339(ts)
        .ok()?
        .with_timezone(&Local)
        .date_naive();
    (first..=today).contains(&day).then_some(day)
}

fn u(val: &Value, key: &str) -> u64 {
    val.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn scan_claude(root: &Path, first: NaiveDate, today: NaiveDate) -> Result<Option<TokenStats>> {
    if !root.is_dir() {
        return Ok(None);
    }
    let mut tally = Tally::default();
    let mut seen = HashSet::new();
    for path in jsonl_files(root, local_midnight(first))? {
        let Ok(file) = std::fs::File::open(&path) else {
            continue;
        };
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            if !line.contains("\"usage\"") {
                continue;
            }
            let Ok(val) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            claude_line(&val, first, today, &mut seen, &mut tally);
        }
    }
    Ok(Some(tally.finish(first, today, None)))
}

fn claude_line(
    val: &Value,
    first: NaiveDate,
    today: NaiveDate,
    seen: &mut HashSet<String>,
    tally: &mut Tally,
) {
    if val.get("type").and_then(Value::as_str) != Some("assistant") {
        return;
    }
    let Some(msg) = val.get("message") else {
        return;
    };
    let Some(usage) = msg.get("usage") else {
        return;
    };
    let model = msg.get("model").and_then(Value::as_str).unwrap_or("");
    if model == "<synthetic>" {
        return;
    }
    let Some(day) = day_of(val, first, today) else {
        return;
    };
    let msg_id = msg.get("id").and_then(Value::as_str);
    let req_id = val.get("requestId").and_then(Value::as_str);
    if let (Some(m), Some(r)) = (msg_id, req_id) {
        if !seen.insert(format!("{m}:{r}")) {
            return;
        }
    }
    let counts = TokenCounts {
        input: u(usage, "input_tokens"),
        output: u(usage, "output_tokens"),
        cache_read: u(usage, "cache_read_input_tokens"),
        cache_write: u(usage, "cache_creation_input_tokens"),
    };
    tally.add(day, model, &counts);
}

/// Codex counts cached input inside `input_tokens`; SAME keeps them apart.
fn codex_counts(usage: &Value) -> TokenCounts {
    let input = u(usage, "input_tokens");
    let cached = u(usage, "cached_input_tokens");
    TokenCounts {
        input: input.saturating_sub(cached),
        output: u(usage, "output_tokens"),
        cache_read: cached,
        cache_write: u(usage, "cache_write_input_tokens"),
    }
}

fn scan_codex(codex_home: &Path, first: NaiveDate, today: NaiveDate) -> Result<Option<TokenStats>> {
    let dirs: Vec<PathBuf> = ["sessions", "archived_sessions"]
        .into_iter()
        .map(|sub| codex_home.join(sub))
        .filter(|d| d.is_dir())
        .collect();
    if dirs.is_empty() {
        return Ok(None);
    }
    let since = local_midnight(first);
    let mut tally = Tally::default();
    let mut seen = HashSet::new();
    for dir in dirs {
        for path in jsonl_files(&dir, since)? {
            let Ok(file) = std::fs::File::open(&path) else {
                continue;
            };
            let lines: Vec<Value> = BufReader::new(file)
                .lines()
                .map_while(Result::ok)
                .filter(|l| {
                    l.contains("\"token_usage_record\"")
                        || l.contains("\"token_count\"")
                        || l.contains("\"turn_context\"")
                })
                .filter_map(|l| serde_json::from_str(&l).ok())
                .collect();
            codex_file(&lines, first, today, &mut seen, &mut tally);
        }
    }
    Ok(Some(tally.finish(first, today, None)))
}

/// One rollout file's lines (only the kinds [`scan_codex`] keeps).
fn codex_file(
    lines: &[Value],
    first: NaiveDate,
    today: NaiveDate,
    seen: &mut HashSet<String>,
    tally: &mut Tally,
) {
    let kind = |v: &Value| {
        v.get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let has_records = lines.iter().any(|v| kind(v) == "token_usage_record");
    let mut model = String::new();
    let mut prev_total: Option<TokenCounts> = None;
    for val in lines {
        let payload = val.get("payload").unwrap_or(&Value::Null);
        match kind(val).as_str() {
            "turn_context" => {
                if let Some(m) = payload.get("model").and_then(Value::as_str) {
                    model = m.to_string();
                }
            }
            "token_usage_record" => {
                let Some(usage) = payload.get("usage") else {
                    continue;
                };
                if let Some(id) = payload.get("response_id").and_then(Value::as_str) {
                    if !seen.insert(id.to_string()) {
                        continue;
                    }
                }
                if let Some(day) = day_of(val, first, today) {
                    tally.add(day, &model, &codex_counts(usage));
                }
            }
            "event_msg" if !has_records => {
                if payload.get("type").and_then(Value::as_str) != Some("token_count") {
                    continue;
                }
                let Some(info) = payload.get("info").filter(|i| i.is_object()) else {
                    continue;
                };
                let Some(total) = info.get("total_token_usage").map(codex_counts) else {
                    continue;
                };
                // The first count of a file may carry an earlier session's
                // totals (a resumed thread); count only its last turn.
                let delta = match prev_total {
                    Some(prev) => total.saturating_sub(&prev),
                    None => info
                        .get("last_token_usage")
                        .map(codex_counts)
                        .unwrap_or_default(),
                };
                prev_total = Some(total);
                if let Some(day) = day_of(val, first, today) {
                    tally.add(day, &model, &delta);
                }
            }
            _ => {}
        }
    }
}

/// `opencode stats --days N` covers the last N days, today included. Each
/// day is the difference of two such windows.
async fn opencode_stats(
    provider: &ProviderConfig,
    target: &ExecTarget,
    cwd: &Path,
    first: NaiveDate,
    today: NaiveDate,
) -> Result<StatsReport> {
    let n = (today - first).num_days() as u32 + 1;
    let whole = opencode_json(provider, target, cwd, n, true).await?;

    const MAX_CONCURRENT_OPENCODE: usize = 4;
    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_OPENCODE));
    let mut windows = tokio::task::JoinSet::new();
    for k in 1..n {
        let (provider, target, cwd, sem) = (
            provider.clone(),
            target.clone(),
            cwd.to_path_buf(),
            sem.clone(),
        );
        windows.spawn(async move {
            let _permit = sem.acquire_owned().await;
            (k, opencode_json(&provider, &target, &cwd, k, false).await)
        });
    }
    let mut cumulative: BTreeMap<u32, TokenCounts> = BTreeMap::new();
    cumulative.insert(n, opencode_counts(whole.get("tokens")));
    while let Some(res) = windows.join_next().await {
        let (k, val) = res?;
        cumulative.insert(k, opencode_counts(val?.get("tokens")));
    }

    let mut tally = Tally::default();
    let mut prev = TokenCounts::default();
    for (k, counts) in &cumulative {
        let day = today
            .checked_sub_days(Days::new(u64::from(k - 1)))
            .unwrap_or(today);
        let delta = counts.saturating_sub(&prev);
        if !delta.is_zero() {
            tally.days.entry(day).or_default().add(&delta);
        }
        prev = *counts;
    }
    for m in whole
        .get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let name = m
            .pointer("/model/id")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let counts = opencode_counts(m.get("tokens"));
        if !counts.is_zero() {
            tally
                .models
                .entry(name.to_string())
                .or_default()
                .add(&counts);
        }
    }
    let cost = whole.get("cost").and_then(Value::as_f64);
    Ok(StatsReport::Available(tally.finish(first, today, cost)))
}

/// OpenCode keeps reasoning apart from output; SAME counts it as output, as
/// Claude Code and Codex do.
fn opencode_counts(tokens: Option<&Value>) -> TokenCounts {
    let Some(t) = tokens else {
        return TokenCounts::default();
    };
    let cache = t.get("cache").unwrap_or(&Value::Null);
    TokenCounts {
        input: u(t, "input"),
        output: u(t, "output") + u(t, "reasoning"),
        cache_read: u(cache, "read"),
        cache_write: u(cache, "write"),
    }
}

async fn opencode_json(
    provider: &ProviderConfig,
    target: &ExecTarget,
    cwd: &Path,
    days: u32,
    models: bool,
) -> Result<Value> {
    let mut args = vec![
        "stats".to_string(),
        "--days".to_string(),
        days.to_string(),
        "--json".to_string(),
    ];
    if models {
        args.push("--models".to_string());
    }
    let mut cmd = build_command(target, &provider.command, &args, cwd, &provider.env).await;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let out = cmd
        .output()
        .await
        .with_context(|| format!("Failed to start '{}'", provider.command))?;
    if !out.status.success() {
        bail!("'{} stats' failed ({})", provider.command, out.status);
    }
    serde_json::from_slice(&out.stdout).context("unexpected 'opencode stats' output")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn d(day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 1, day).unwrap()
    }

    /// Noon local time, so the parsed local date matches `day` in every time zone.
    fn ts(day: u32) -> String {
        Local
            .with_ymd_and_hms(2026, 1, day, 12, 0, 0)
            .single()
            .unwrap()
            .to_rfc3339()
    }

    #[test]
    fn claude_dedupes_repeated_message_lines() {
        let mut tally = Tally::default();
        let mut seen = HashSet::new();
        let line = |id: &str, req: &str, day: u32| {
            json!({"type": "assistant", "timestamp": ts(day), "requestId": req,
                   "message": {"id": id, "model": "claude-opus-5-5", "usage": {
                       "input_tokens": 2, "output_tokens": 10,
                       "cache_read_input_tokens": 100, "cache_creation_input_tokens": 50}}})
        };
        for v in [
            line("msg_1", "req_1", 5),
            line("msg_1", "req_1", 5), // the same reply, another content block
            line("msg_2", "req_2", 6),
            line("msg_3", "req_3", 1), // before the period
            json!({"type": "assistant", "timestamp": ts(6),
                   "message": {"model": "<synthetic>", "usage": {"output_tokens": 9}}}),
            json!({"type": "user", "timestamp": ts(6), "message": {"usage": {"input_tokens": 9}}}),
        ] {
            claude_line(&v, d(4), d(6), &mut seen, &mut tally);
        }
        let stats = tally.finish(d(4), d(6), None);
        assert_eq!(stats.days.len(), 3);
        assert!(stats.days[0].1.is_zero());
        assert_eq!(
            stats.days[1].1,
            TokenCounts {
                input: 2,
                output: 10,
                cache_read: 100,
                cache_write: 50
            }
        );
        assert_eq!(stats.total.total(), 2 * 162);
        assert_eq!(stats.models, vec![("claude-opus-5-5".into(), stats.total)]);
    }

    #[test]
    fn codex_records_are_deduped_by_response() {
        let rec = |id: &str, day: u32, input: u64| {
            json!({"timestamp": ts(day), "type": "token_usage_record", "payload": {
                "response_id": id,
                "usage": {"input_tokens": input, "cached_input_tokens": 40,
                          "cache_write_input_tokens": 0, "output_tokens": 5}}})
        };
        let file = vec![
            json!({"type": "turn_context", "payload": {"model": "gpt-6.1-sol"}}),
            rec("resp_1", 5, 100),
            rec("resp_2", 6, 200),
            // Ignored: the file has records.
            json!({"timestamp": ts(6), "type": "event_msg", "payload": {"type": "token_count",
                "info": {"total_token_usage": {"input_tokens": 999999}}}}),
        ];
        let mut tally = Tally::default();
        let mut seen = HashSet::new();
        codex_file(&file, d(5), d(6), &mut seen, &mut tally);
        // A resumed copy of the same responses adds nothing.
        codex_file(&file, d(5), d(6), &mut seen, &mut tally);
        let stats = tally.finish(d(5), d(6), None);
        assert_eq!(
            stats.days[0].1,
            TokenCounts {
                input: 60,
                output: 5,
                cache_read: 40,
                cache_write: 0
            }
        );
        assert_eq!(stats.days[1].1.input, 160);
        assert_eq!(stats.models[0].0, "gpt-6.1-sol");
    }

    #[test]
    fn codex_old_files_use_growth_of_totals() {
        let count = |day: u32, total_in: u64, last_in: u64| {
            json!({"timestamp": ts(day), "type": "event_msg", "payload": {"type": "token_count",
                "info": {"total_token_usage": {"input_tokens": total_in, "output_tokens": 0},
                         "last_token_usage": {"input_tokens": last_in, "output_tokens": 0}}}})
        };
        let file = vec![
            count(5, 1000, 100), // resumed: only the last turn counts
            json!({"timestamp": ts(5), "type": "event_msg",
                   "payload": {"type": "token_count", "info": null}}),
            count(5, 1300, 300),
            count(6, 1300, 0), // repeated totals add nothing
            count(6, 1700, 400),
        ];
        let mut tally = Tally::default();
        codex_file(&file, d(5), d(6), &mut HashSet::new(), &mut tally);
        let stats = tally.finish(d(5), d(6), None);
        assert_eq!(stats.days[0].1.input, 400);
        assert_eq!(stats.days[1].1.input, 400);
        assert_eq!(stats.models[0].0, "unknown");
    }

    #[test]
    fn opencode_counts_reasoning_as_output() {
        let t = json!({"input": 7, "output": 3, "reasoning": 2, "cache": {"read": 50, "write": 1}});
        assert_eq!(
            opencode_counts(Some(&t)),
            TokenCounts {
                input: 7,
                output: 5,
                cache_read: 50,
                cache_write: 1
            }
        );
    }

    #[test]
    fn sources_by_kind_and_command() {
        let p = |kind, command: &str| ProviderConfig {
            kind,
            command: command.into(),
            ..same_core::config::default_providers()[0].clone()
        };
        assert_eq!(
            stats_source(&p(ProviderKind::ClaudeCli, "claude")),
            Some(StatsSource::Claude)
        );
        assert_eq!(
            stats_source(&p(ProviderKind::Acp, "/usr/bin/opencode")),
            Some(StatsSource::OpenCode)
        );
        assert_eq!(stats_source(&p(ProviderKind::Acp, "agy")), None);
    }
}
