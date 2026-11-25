use anyhow::{anyhow, Context, Result};
use same_acp::exec::{build_command, ExecTarget};
use same_core::{EventBus, ProviderConfig, SameEvent, ToolCallRecord, ToolCallStatus};
use serde_json::Value;
use std::path::Path;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::oneshot;

/// Collects a command line so it can be launched in any [`ExecTarget`].
struct ArgList(Vec<String>);

impl ArgList {
    fn new(base: &[String]) -> Self {
        Self(base.to_vec())
    }

    fn arg(&mut self, a: impl Into<String>) -> &mut Self {
        self.0.push(a.into());
        self
    }
}

pub async fn run_claude_cli(
    provider: &ProviderConfig,
    session_id: &str,
    external_session_id: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
    cwd: &Path,
    target: &ExecTarget,
    prompt: &str,
    auto_approve: bool,
    events: EventBus,
    mut cancel_rx: oneshot::Receiver<()>,
) -> Result<()> {
    let mut cmd = ArgList::new(&provider.args);
    cmd.arg("-p")
        .arg("--output-format")
        .arg("stream-json")
        .arg("--verbose");

    if auto_approve {
        cmd.arg("--permission-mode").arg("acceptEdits");
    }
    if let Some(m) = model.filter(|s| !s.trim().is_empty()) {
        cmd.arg("--model").arg(m);
    }
    if let Some(e) = effort.filter(|s| !s.trim().is_empty()) {
        cmd.arg("--effort").arg(e);
    }
    if let Some(ext) = external_session_id.filter(|s| !s.trim().is_empty()) {
        cmd.arg("--resume").arg(ext);
    }

    cmd.arg(prompt);
    let mut cmd = build_command(target, &provider.command, &cmd.0, cwd, &provider.env).await;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .with_context(|| format!("Failed to spawn '{}'", provider.command))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("Failed to capture Claude stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("Failed to capture Claude stderr"))?;

    {
        let events = events.clone();
        let sid = session_id.to_string();
        let pid = provider.id.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                events.publish(SameEvent::AgentLog {
                    session_id: Some(sid.clone()),
                    provider_id: pid.clone(),
                    line,
                });
            }
        });
    }

    let mut lines = BufReader::new(stdout).lines();
    let mut emitted_any_text = false;

    loop {
        tokio::select! {
            _ = &mut cancel_rx => {
                let _ = child.kill().await;
                events.publish(SameEvent::SessionCancelled {
                    session_id: session_id.to_string(),
                });
                return Ok(());
            }
            line_res = lines.next_line() => {
                match line_res? {
                    Some(line) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
                            handle_claude_json(&val, session_id, &events, &mut emitted_any_text);
                        } else {
                            events.publish(SameEvent::AgentLog {
                                session_id: Some(session_id.to_string()),
                                provider_id: provider.id.clone(),
                                line: trimmed.to_string(),
                            });
                        }
                    }
                    None => break,
                }
            }
        }
    }

    let status = child.wait().await?;
    if !status.success() {
        return Err(anyhow!(
            "Claude CLI exited with status {}",
            status.code().unwrap_or(-1)
        ));
    }

    events.publish(SameEvent::SessionCompleted {
        session_id: session_id.to_string(),
        stop_reason: Some("end_turn".to_string()),
    });
    Ok(())
}

fn handle_claude_json(
    val: &Value,
    session_id: &str,
    events: &EventBus,
    emitted_any_text: &mut bool,
) {
    if let Some(ext_id) = val.get("session_id").and_then(Value::as_str) {
        if !ext_id.is_empty() {
            events.publish(SameEvent::ExternalSessionBound {
                session_id: session_id.to_string(),
                external_session_id: ext_id.to_string(),
            });
        }
    }

    let msg_type = val.get("type").and_then(Value::as_str).unwrap_or("");
    match msg_type {
        "assistant" => {
            if let Some(content) = val
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(Value::as_array)
            {
                for block in content {
                    let btype = block.get("type").and_then(Value::as_str).unwrap_or("");
                    match btype {
                        "text" => {
                            if let Some(text) = block.get("text").and_then(Value::as_str) {
                                *emitted_any_text = true;
                                events.publish(SameEvent::MessageDelta {
                                    session_id: session_id.to_string(),
                                    text: text.to_string(),
                                });
                            }
                        }
                        "thinking" => {
                            if let Some(t) = block.get("thinking").and_then(Value::as_str) {
                                events.publish(SameEvent::ThinkingDelta {
                                    session_id: session_id.to_string(),
                                    text: t.to_string(),
                                });
                            }
                        }
                        "tool_use" => {
                            let id = block
                                .get("id")
                                .and_then(Value::as_str)
                                .unwrap_or("tool")
                                .to_string();
                            let name = block
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("Tool")
                                .to_string();
                            let title = summarize_tool_call(&name, block.get("input"));
                            let input = block
                                .get("input")
                                .map(|v| serde_json::to_string_pretty(v).unwrap_or_default());
                            events.publish(SameEvent::ToolCallStarted {
                                session_id: session_id.to_string(),
                                tool_call: ToolCallRecord {
                                    id,
                                    title,
                                    kind: None,
                                    status: ToolCallStatus::InProgress,
                                    input,
                                    output: None,
                                },
                            });
                        }
                        _ => {}
                    }
                }
            }
        }
        "user" => {
            if let Some(content) = val
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(Value::as_array)
            {
                for block in content {
                    if block.get("type").and_then(Value::as_str) == Some("tool_result") {
                        let id = block
                            .get("tool_use_id")
                            .and_then(Value::as_str)
                            .unwrap_or("tool")
                            .to_string();
                        let is_err = block
                            .get("is_error")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        let output = block.get("content").map(|v| {
                            if let Some(s) = v.as_str() {
                                s.to_string()
                            } else {
                                serde_json::to_string_pretty(v).unwrap_or_default()
                            }
                        });
                        events.publish(SameEvent::ToolCallUpdated {
                            session_id: session_id.to_string(),
                            tool_call: ToolCallRecord {
                                id,
                                title: String::new(),
                                kind: None,
                                status: if is_err {
                                    ToolCallStatus::Failed
                                } else {
                                    ToolCallStatus::Completed
                                },
                                input: None,
                                output,
                            },
                        });
                    }
                }
            }
        }
        "result" => {
            if !*emitted_any_text {
                if let Some(res) = val.get("result").and_then(Value::as_str) {
                    if !res.is_empty() {
                        *emitted_any_text = true;
                        events.publish(SameEvent::MessageDelta {
                            session_id: session_id.to_string(),
                            text: res.to_string(),
                        });
                    }
                }
            }
        }
        _ => {}
    }
}

pub async fn run_codex_cli(
    provider: &ProviderConfig,
    session_id: &str,
    external_session_id: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
    cwd: &Path,
    target: &ExecTarget,
    prompt: &str,
    auto_approve: bool,
    events: EventBus,
    mut cancel_rx: oneshot::Receiver<()>,
) -> Result<()> {
    let mut cmd = ArgList::new(&provider.args);
    cmd.arg("exec");

    if let Some(ext) = external_session_id.filter(|s| !s.trim().is_empty()) {
        cmd.arg("resume").arg(ext);
    }

    cmd.arg("--json").arg("--skip-git-repo-check");

    if auto_approve {
        cmd.arg("--full-auto");
    }
    if let Some(m) = model.filter(|s| !s.trim().is_empty()) {
        cmd.arg("-m").arg(m);
    }
    if let Some(e) = effort.filter(|s| !s.trim().is_empty()) {
        cmd.arg("-c").arg(format!("model_reasoning_effort=\"{e}\""));
    }

    cmd.arg(prompt);
    let mut cmd = build_command(target, &provider.command, &cmd.0, cwd, &provider.env).await;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .with_context(|| format!("Failed to spawn '{}'", provider.command))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("Failed to capture Codex stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("Failed to capture Codex stderr"))?;

    {
        let events = events.clone();
        let sid = session_id.to_string();
        let pid = provider.id.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                events.publish(SameEvent::AgentLog {
                    session_id: Some(sid.clone()),
                    provider_id: pid.clone(),
                    line,
                });
            }
        });
    }

    let mut lines = BufReader::new(stdout).lines();

    loop {
        tokio::select! {
            _ = &mut cancel_rx => {
                let _ = child.kill().await;
                events.publish(SameEvent::SessionCancelled {
                    session_id: session_id.to_string(),
                });
                return Ok(());
            }
            line_res = lines.next_line() => {
                match line_res? {
                    Some(line) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
                            handle_codex_json(&val, session_id, &events);
                        } else {
                            events.publish(SameEvent::AgentLog {
                                session_id: Some(session_id.to_string()),
                                provider_id: provider.id.clone(),
                                line: trimmed.to_string(),
                            });
                        }
                    }
                    None => break,
                }
            }
        }
    }

    let status = child.wait().await?;
    if !status.success() {
        return Err(anyhow!(
            "Codex CLI exited with status {}",
            status.code().unwrap_or(-1)
        ));
    }

    events.publish(SameEvent::SessionCompleted {
        session_id: session_id.to_string(),
        stop_reason: Some("end_turn".to_string()),
    });
    Ok(())
}

fn handle_codex_json(val: &Value, session_id: &str, events: &EventBus) {
    let ev_type = val.get("type").and_then(Value::as_str).unwrap_or("");
    match ev_type {
        "thread.started" => {
            if let Some(tid) = val.get("thread_id").and_then(Value::as_str) {
                events.publish(SameEvent::ExternalSessionBound {
                    session_id: session_id.to_string(),
                    external_session_id: tid.to_string(),
                });
            }
        }
        "item.started" | "item.updated" | "item.completed" => {
            let Some(item) = val.get("item") else {
                return;
            };
            let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
            let item_id = item
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("item")
                .to_string();

            match item_type {
                "agent_message" if ev_type == "item.completed" => {
                    if let Some(text) = item.get("text").and_then(Value::as_str) {
                        events.publish(SameEvent::MessageDelta {
                            session_id: session_id.to_string(),
                            text: text.to_string(),
                        });
                    }
                }
                "reasoning" if ev_type == "item.completed" => {
                    if let Some(text) = item.get("text").and_then(Value::as_str) {
                        events.publish(SameEvent::ThinkingDelta {
                            session_id: session_id.to_string(),
                            text: text.to_string(),
                        });
                    }
                }
                "command_execution" => {
                    let command = item
                        .get("command")
                        .and_then(Value::as_str)
                        .unwrap_or("Command")
                        .to_string();
                    let output = item
                        .get("aggregated_output")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    let status = if ev_type == "item.completed" {
                        let code = item.get("exit_code").and_then(Value::as_i64).unwrap_or(0);
                        if code == 0 {
                            ToolCallStatus::Completed
                        } else {
                            ToolCallStatus::Failed
                        }
                    } else {
                        ToolCallStatus::InProgress
                    };
                    let rec = ToolCallRecord {
                        id: item_id,
                        title: format!("Shell: {command}"),
                        kind: Some("command_execution".to_string()),
                        status,
                        input: Some(command),
                        output,
                    };
                    if ev_type == "item.started" {
                        events.publish(SameEvent::ToolCallStarted {
                            session_id: session_id.to_string(),
                            tool_call: rec,
                        });
                    } else {
                        events.publish(SameEvent::ToolCallUpdated {
                            session_id: session_id.to_string(),
                            tool_call: rec,
                        });
                    }
                }
                "file_change" => {
                    let output = serde_json::to_string_pretty(item).ok();
                    let rec = ToolCallRecord {
                        id: item_id,
                        title: "File change".to_string(),
                        kind: Some("file_change".to_string()),
                        status: if ev_type == "item.completed" {
                            ToolCallStatus::Completed
                        } else {
                            ToolCallStatus::InProgress
                        },
                        input: None,
                        output,
                    };
                    if ev_type == "item.started" {
                        events.publish(SameEvent::ToolCallStarted {
                            session_id: session_id.to_string(),
                            tool_call: rec,
                        });
                    } else {
                        events.publish(SameEvent::ToolCallUpdated {
                            session_id: session_id.to_string(),
                            tool_call: rec,
                        });
                    }
                }
                _ => {}
            }
        }
        "error" => {
            if let Some(msg) = val.get("message").and_then(Value::as_str) {
                events.publish(SameEvent::SessionFailed {
                    session_id: session_id.to_string(),
                    error: msg.to_string(),
                });
            }
        }
        _ => {}
    }
}

/// Builds a one-line tool card title such as `Read src/main.rs` or
/// `Bash: cargo test` from a Claude `tool_use` block.
fn summarize_tool_call(name: &str, input: Option<&Value>) -> String {
    let field = |k: &str| {
        input
            .and_then(|i| i.get(k))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    let detail = field("file_path")
        .or_else(|| field("notebook_path"))
        .or_else(|| field("path"))
        .or_else(|| field("command"))
        .or_else(|| field("pattern"))
        .or_else(|| field("url"))
        .or_else(|| field("query"))
        .or_else(|| field("description"));
    match detail {
        Some(d) => {
            let first = d.lines().next().unwrap_or(d);
            let mut short: String = first.chars().take(80).collect();
            if first.chars().count() > 80 || d.lines().count() > 1 {
                short.push('…');
            }
            format!("{name} {short}")
        }
        None => name.to_string(),
    }
}

#[cfg(test)]
mod summarize_tests {
    use super::summarize_tool_call;
    use serde_json::json;

    #[test]
    fn summarizes_common_tool_inputs() {
        assert_eq!(
            summarize_tool_call("Read", Some(&json!({"file_path": "src/main.rs"}))),
            "Read src/main.rs"
        );
        assert_eq!(
            summarize_tool_call("Bash", Some(&json!({"command": "cargo test\necho hi"}))),
            "Bash cargo test…"
        );
        assert_eq!(
            summarize_tool_call("TodoWrite", Some(&json!({"todos": []}))),
            "TodoWrite"
        );
        assert_eq!(summarize_tool_call("X", None), "X");
    }
}
