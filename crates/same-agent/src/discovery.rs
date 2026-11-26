//! Asks each agent which models it offers, so the model picker lists what the
//! provider really supports instead of a hard-coded guess.
//!
//! - ACP agents: the model list in the `session/new` response.
//! - Claude Code: the `models` in the stream-json `initialize` control response
//!   (the same list the Agent SDK's `supportedModels()` reads).
//! - Codex: `model/list` on `codex app-server`.

use anyhow::{anyhow, bail, Context, Result};
use same_acp::exec::{build_command, ExecTarget};
use same_acp::{AcpClient, AcpClientConfig, AcpModelState};
use same_core::{ModelInfo, ProviderConfig, ProviderKind};
use serde_json::{json, Value};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(30);

/// What a provider reported about its models.
#[derive(Debug, Clone, Default)]
pub struct Discovered {
    pub models: Vec<ModelInfo>,
    /// The model the agent uses when none is picked (an id in `models`).
    pub default_model: Option<String>,
    /// For ACP agents: a fresh session's model state, i.e. the agent's own
    /// default model and effort.
    pub acp_defaults: Option<AcpModelState>,
}

/// Returns the models `provider` offers when run in `target`.
pub async fn discover_models(
    provider: &ProviderConfig,
    target: &ExecTarget,
    cwd: &Path,
) -> Result<Discovered> {
    let fut = async {
        match provider.kind {
            ProviderKind::Acp => discover_acp(provider, target, cwd).await,
            ProviderKind::ClaudeCli => discover_claude(provider, target, cwd).await,
            ProviderKind::CodexCli => discover_codex(provider, target, cwd).await,
        }
    };
    tokio::time::timeout(DISCOVERY_TIMEOUT, fut)
        .await
        .map_err(|_| anyhow!("timed out"))?
}

async fn discover_acp(
    provider: &ProviderConfig,
    target: &ExecTarget,
    cwd: &Path,
) -> Result<Discovered> {
    let (client, _rx) = AcpClient::spawn(AcpClientConfig {
        command: provider.command.clone(),
        args: provider.args.clone(),
        env: provider.env.clone(),
        cwd: cwd.to_path_buf(),
        auto_approve: false,
        target: target.clone(),
    })
    .await?;
    let res = client.new_session(cwd).await;
    client.shutdown().await;
    let state = res?.model_state.unwrap_or_default();
    // ACP agents report one effort list, for the session's (default) model.
    let efforts: Vec<String> = state.efforts.iter().map(|e| e.id.clone()).collect();
    Ok(Discovered {
        models: state
            .models
            .iter()
            .map(|m| ModelInfo {
                id: m.id.clone(),
                name: m.name.clone(),
                description: m.description.clone(),
                efforts: efforts.clone(),
                // Known only for the model the session started on.
                default_effort: (state.current.as_deref() == Some(m.id.as_str()))
                    .then(|| state.current_effort.clone())
                    .flatten()
                    .filter(|e| e != "default"),
            })
            .collect(),
        default_model: state.current.clone(),
        acp_defaults: Some(state),
    })
}

/// Spawns `provider.command` with `args` (after the provider's own args),
/// writes `requests` as JSON lines, and returns the first stdout line for
/// which `pick` returns `Some`.
pub(crate) async fn json_lines_exchange<T>(
    provider: &ProviderConfig,
    target: &ExecTarget,
    cwd: &Path,
    args: &[&str],
    requests: &[Value],
    mut pick: impl FnMut(&Value) -> Option<Result<T>>,
) -> Result<T> {
    let mut all_args = provider.args.clone();
    all_args.extend(args.iter().map(|s| s.to_string()));
    let mut cmd = build_command(target, &provider.command, &all_args, cwd, &provider.env).await;
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = cmd
        .spawn()
        .with_context(|| format!("Failed to start '{}'", provider.command))?;
    let mut stdin = child.stdin.take().context("no stdin")?;
    let stdout = child.stdout.take().context("no stdout")?;

    for req in requests {
        let mut line = serde_json::to_string(req)?;
        line.push('\n');
        stdin.write_all(line.as_bytes()).await?;
    }
    stdin.flush().await?;

    let mut lines = BufReader::new(stdout).lines();
    let result = loop {
        let Some(line) = lines.next_line().await? else {
            break Err(anyhow!("'{}' exited without answering", provider.command));
        };
        let Ok(val) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(r) = pick(&val) {
            break r;
        }
    };

    // Closing stdin lets the agent exit on its own; through `wsl.exe` a kill
    // may not reach the Linux process.
    drop(stdin);
    if tokio::time::timeout(Duration::from_secs(3), child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
    }
    result
}

async fn discover_claude(
    provider: &ProviderConfig,
    target: &ExecTarget,
    cwd: &Path,
) -> Result<Discovered> {
    const REQ_ID: &str = "same-models";
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
        &[json!({
            "type": "control_request",
            "request_id": REQ_ID,
            "request": { "subtype": "initialize" }
        })],
        |val| {
            let resp = val.get("response")?;
            if val.get("type")?.as_str()? != "control_response"
                || resp.get("request_id")?.as_str()? != REQ_ID
            {
                return None;
            }
            Some(parse_claude_models(resp))
        },
    )
    .await
}

fn parse_claude_models(resp: &Value) -> Result<Discovered> {
    if resp.get("subtype").and_then(Value::as_str) == Some("error") {
        bail!(
            "{}",
            resp.get("error")
                .and_then(Value::as_str)
                .unwrap_or("initialize failed")
        );
    }
    let models = resp
        .pointer("/response/models")
        .and_then(Value::as_array)
        .context("no models in initialize response")?;
    let resolved = |m: &Value| {
        m.get("resolvedModel")
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    // The "default" entry names the model it resolves to; point at the
    // listed alias for the same model (e.g. "opus").
    let default_target = models
        .iter()
        .find(|m| m.get("value").and_then(Value::as_str) == Some("default"))
        .and_then(resolved);
    let default_model = default_target.and_then(|target| {
        models
            .iter()
            .filter(|m| m.get("value").and_then(Value::as_str) != Some("default"))
            .find(|m| resolved(m).as_deref() == Some(target.as_str()))
            .and_then(|m| m.get("value").and_then(Value::as_str).map(str::to_string))
    });
    let models = models
        .iter()
        .filter_map(|m| {
            let id = m.get("value")?.as_str()?;
            // SAME's picker has its own "Default" row.
            if id == "default" {
                return None;
            }
            Some(ModelInfo {
                id: id.to_string(),
                name: m
                    .get("displayName")
                    .and_then(Value::as_str)
                    .unwrap_or(id)
                    .to_string(),
                description: m
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                efforts: m
                    .get("supportedEffortLevels")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default(),
                default_effort: None,
            })
        })
        .collect();
    Ok(Discovered {
        models,
        default_model,
        acp_defaults: None,
    })
}

async fn discover_codex(
    provider: &ProviderConfig,
    target: &ExecTarget,
    cwd: &Path,
) -> Result<Discovered> {
    json_lines_exchange(
        provider,
        target,
        cwd,
        &["app-server"],
        &[
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                   "params": {"clientInfo": {"name": "same", "version": env!("CARGO_PKG_VERSION")}}}),
            json!({"jsonrpc": "2.0", "method": "initialized"}),
            json!({"jsonrpc": "2.0", "id": 2, "method": "model/list", "params": {"limit": 500}}),
        ],
        |val| {
            if val.get("id")?.as_i64()? != 2 {
                return None;
            }
            Some(parse_codex_models(val))
        },
    )
    .await
}

fn parse_codex_models(val: &Value) -> Result<Discovered> {
    if let Some(err) = val.get("error") {
        bail!(
            "{}",
            err.get("message")
                .and_then(Value::as_str)
                .unwrap_or("model/list failed")
        );
    }
    let data = val
        .pointer("/result/data")
        .and_then(Value::as_array)
        .context("no data in model/list response")?;
    let visible: Vec<&Value> = data
        .iter()
        .filter(|m| !m.get("hidden").and_then(Value::as_bool).unwrap_or(false))
        .collect();
    let model_id = |m: &Value| {
        m.get("model")
            .or_else(|| m.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let default_model = visible
        .iter()
        .find(|m| m.get("isDefault").and_then(Value::as_bool) == Some(true))
        .and_then(|m| model_id(m));
    let models = visible
        .into_iter()
        .filter_map(|m| {
            let id = m
                .get("model")
                .or_else(|| m.get("id"))
                .and_then(Value::as_str)?;
            Some(ModelInfo {
                id: id.to_string(),
                name: m
                    .get("displayName")
                    .and_then(Value::as_str)
                    .unwrap_or(id)
                    .to_string(),
                description: m
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                efforts: m
                    .get("supportedReasoningEfforts")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|e| e.get("reasoningEffort").and_then(Value::as_str))
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default(),
                default_effort: m
                    .get("defaultReasoningEffort")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
        })
        .collect();
    Ok(Discovered {
        models,
        default_model,
        acp_defaults: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_models_skip_default() {
        let resp = json!({
            "subtype": "success",
            "request_id": "same-models",
            "response": {"models": [
                {"value": "default", "displayName": "Default (recommended)", "description": "Opus",
                 "resolvedModel": "claude-opus-5-5"},
                {"value": "opus", "displayName": "Opus 5.5", "description": "Complex work",
                 "resolvedModel": "claude-opus-5-5",
                 "supportedEffortLevels": ["low", "high"]},
                {"value": "claude-haiku-4-5-20251001", "displayName": "Haiku 4.5"}
            ]}
        });
        let found = parse_claude_models(&resp).unwrap();
        assert_eq!(found.default_model.as_deref(), Some("opus"));
        let ms = found.models;
        assert_eq!(
            ms.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["opus", "claude-haiku-4-5-20251001"]
        );
        assert_eq!(ms[0].name, "Opus 5.5");
        assert_eq!(ms[0].efforts, ["low", "high"]);
        assert!(ms[1].efforts.is_empty());
        assert_eq!(ms[1].description, "");
    }

    #[test]
    fn claude_error_is_reported() {
        let resp = json!({"subtype": "error", "request_id": "same-models", "error": "boom"});
        assert!(parse_claude_models(&resp).is_err());
    }

    #[test]
    fn codex_models_skip_hidden() {
        let val = json!({"id": 2, "result": {"nextCursor": null, "data": [
            {"id": "gpt-6.2-sol", "model": "gpt-6.2-sol", "displayName": "GPT-6.2-Sol",
             "description": "Fast", "hidden": false, "isDefault": true,
             "supportedReasoningEfforts": [{"reasoningEffort": "low"}, {"reasoningEffort": "high"}],
             "defaultReasoningEffort": "low"},
            {"id": "gpt-reserve", "model": "gpt-reserve", "displayName": "Reserve", "hidden": true}
        ]}});
        let found = parse_codex_models(&val).unwrap();
        assert_eq!(found.default_model.as_deref(), Some("gpt-6.2-sol"));
        let ms = found.models;
        assert_eq!(ms.len(), 1);
        assert_eq!(ms[0].id, "gpt-6.2-sol");
        assert_eq!(ms[0].name, "GPT-6.2-Sol");
        assert_eq!(ms[0].efforts, ["low", "high"]);
        assert_eq!(ms[0].default_effort.as_deref(), Some("low"));
    }
}
