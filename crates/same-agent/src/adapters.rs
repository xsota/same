use anyhow::{anyhow, Context, Result};
use same_acp::exec::{build_command, ExecTarget};
use same_core::{EventBus, ProviderConfig, SameEvent, ToolCallRecord, ToolCallStatus};
use serde_json::Value;
use std::collections::VecDeque;
use std::path::Path;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Child;
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

/// Sends `prompt` to the child's stdin and closes it, so the CLI sees the
/// end of input. Written in the background: a large prompt must not block
/// reading the child's output. The returned handle resolves to any I/O error
/// so the caller can surface incomplete writes when the child exits zero.
fn write_prompt_to_stdin(
    child: &mut Child,
    prompt: &str,
) -> Result<tokio::task::JoinHandle<Result<()>>> {
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("Failed to capture agent stdin"))?;
    let prompt = prompt.to_string();
    Ok(tokio::spawn(async move {
        stdin
            .write_all(prompt.as_bytes())
            .await
            .context("Failed to write prompt to agent stdin")?;
        stdin
            .shutdown()
            .await
            .context("Failed to close agent stdin")?;
        Ok(())
    }))
}

/// Whether an attachment is an image the agent can look at directly.
pub fn is_image_path(path: &str) -> bool {
    let ext = path
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    matches!(
        ext.as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp"
    )
}

/// Appends attachments as Claude Code `@path` mentions, which it expands
/// into the files' contents (images included). `paths` are in the agent's
/// terms (see [`ExecTarget::to_target_path`]).
pub fn claude_prompt_with_attachments(prompt: &str, paths: &[String]) -> String {
    if paths.is_empty() {
        return prompt.to_string();
    }
    let mentions: Vec<String> = paths
        .iter()
        .map(|p| {
            if p.contains(char::is_whitespace) {
                format!("@\"{p}\"")
            } else {
                format!("@{p}")
            }
        })
        .collect();
    format!("{prompt}\n\n{}", mentions.join("\n"))
}

/// Appends the attachment paths for Codex, which reads files with its own
/// tools; images are also passed with `--image`.
pub fn codex_prompt_with_attachments(prompt: &str, paths: &[String]) -> String {
    if paths.is_empty() {
        return prompt.to_string();
    }
    let list: Vec<String> = paths.iter().map(|p| format!("- {p}")).collect();
    format!("{prompt}\n\nAttached files:\n{}", list.join("\n"))
}

/// Lines of stderr kept to explain a failed run.
const STDERR_TAIL_LINES: usize = 20;

/// How one `claude -p` process ended.
enum ClaudeRun {
    Completed,
    Cancelled,
    Failed {
        code: Option<i32>,
        /// `errors` of an `is_error` result line.
        errors: Vec<String>,
        stderr_tail: Vec<String>,
        /// True if the turn already emitted assistant output or tool activity.
        did_work: bool,
    },
}

impl ClaudeRun {
    /// Claude Code could not find the conversation passed to `--resume`
    /// (deleted, or created in another directory or environment) before doing
    /// any work on this turn.
    fn is_missing_conversation(&self, resume_id: &str) -> bool {
        match self {
            ClaudeRun::Failed {
                errors,
                stderr_tail,
                did_work: false,
                ..
            } => errors
                .iter()
                .chain(stderr_tail)
                .any(|l| l.contains("No conversation found") && l.contains(resume_id)),
            _ => false,
        }
    }
}

/// Builds the user-facing error of a failed run, with the reason Claude
/// Code gave.
fn claude_failure_message(code: Option<i32>, errors: &[String], stderr_tail: &[String]) -> String {
    let mut msg = format!("Claude CLI exited with status {}", code.unwrap_or(-1));
    let detail: Vec<&str> = if errors.is_empty() {
        stderr_tail
            .iter()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .collect()
    } else {
        errors.iter().map(String::as_str).collect()
    };
    if !detail.is_empty() {
        msg.push_str(": ");
        msg.push_str(&detail.join("\n"));
    }
    msg
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
    let mut resume = external_session_id.filter(|s| !s.trim().is_empty());
    loop {
        let run = run_claude_once(
            provider,
            session_id,
            resume,
            model,
            effort,
            cwd,
            target,
            prompt,
            auto_approve,
            &events,
            &mut cancel_rx,
        )
        .await?;
        match run {
            ClaudeRun::Completed => {
                events.publish(SameEvent::SessionCompleted {
                    session_id: session_id.to_string(),
                    stop_reason: Some("end_turn".to_string()),
                });
                return Ok(());
            }
            ClaudeRun::Cancelled => {
                events.publish(SameEvent::SessionCancelled {
                    session_id: session_id.to_string(),
                });
                return Ok(());
            }
            ref failed if resume.is_some_and(|id| failed.is_missing_conversation(id)) => {
                // The stored conversation is gone: start a fresh one. Its
                // new id replaces the stale one through ExternalSessionBound.
                events.publish(SameEvent::AgentLog {
                    session_id: Some(session_id.to_string()),
                    provider_id: provider.id.clone(),
                    line: format!(
                        "Conversation {} not found; starting a new Claude session",
                        resume.unwrap_or_default()
                    ),
                });
                resume = None;
            }
            ClaudeRun::Failed {
                code,
                errors,
                stderr_tail,
                ..
            } => return Err(anyhow!(claude_failure_message(code, &errors, &stderr_tail))),
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_claude_once(
    provider: &ProviderConfig,
    session_id: &str,
    resume: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
    cwd: &Path,
    target: &ExecTarget,
    prompt: &str,
    auto_approve: bool,
    events: &EventBus,
    cancel_rx: &mut oneshot::Receiver<()>,
) -> Result<ClaudeRun> {
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
    if let Some(ext) = resume {
        cmd.arg("--resume").arg(ext);
    }

    // The prompt goes through stdin: as an argument, text starting with
    // `-` would be parsed as an option.
    let mut cmd = build_command(target, &provider.command, &cmd.0, cwd, &provider.env).await;
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .with_context(|| format!("Failed to spawn '{}'", provider.command))?;
    let stdin_task = write_prompt_to_stdin(&mut child, prompt)?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("Failed to capture Claude stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("Failed to capture Claude stderr"))?;

    let stderr_tail_buf = std::sync::Arc::new(std::sync::Mutex::new(VecDeque::new()));
    let mut stderr_task = {
        let events = events.clone();
        let sid = session_id.to_string();
        let pid = provider.id.clone();
        let tail_buf = stderr_tail_buf.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                {
                    let mut tail = tail_buf.lock().unwrap();
                    if tail.len() == STDERR_TAIL_LINES {
                        tail.pop_front();
                    }
                    tail.push_back(line.clone());
                }
                events.publish(SameEvent::AgentLog {
                    session_id: Some(sid.clone()),
                    provider_id: pid.clone(),
                    line,
                });
            }
        })
    };

    let mut lines = BufReader::new(stdout).lines();
    let mut state = ClaudeStreamState::default();

    loop {
        tokio::select! {
            _ = &mut *cancel_rx => {
                let _ = child.kill().await;
                return Ok(ClaudeRun::Cancelled);
            }
            line_res = lines.next_line() => {
                match line_res? {
                    Some(line) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
                            handle_claude_json(&val, session_id, events, &mut state);
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
    // Give stderr a brief moment to drain after the child exits, without
    // hanging indefinitely if a background subprocess inherited the pipe.
    if tokio::time::timeout(std::time::Duration::from_millis(200), &mut stderr_task)
        .await
        .is_err()
    {
        stderr_task.abort();
    }
    let stderr_tail: Vec<String> = stderr_tail_buf.lock().unwrap().iter().cloned().collect();
    // An error result with a zero exit still failed the turn.
    if !status.success() || state.is_error || !state.errors.is_empty() {
        return Ok(ClaudeRun::Failed {
            code: status.code(),
            errors: state.errors,
            stderr_tail,
            did_work: state.did_work,
        });
    }
    if let Ok(Err(err)) = stdin_task.await {
        return Err(err);
    }
    Ok(ClaudeRun::Completed)
}

/// What a `stream-json` run has reported so far.
#[derive(Default)]
struct ClaudeStreamState {
    emitted_any_text: bool,
    did_work: bool,
    is_error: bool,
    /// `errors` of an `is_error` result line.
    errors: Vec<String>,
}

fn handle_claude_json(
    val: &Value,
    session_id: &str,
    events: &EventBus,
    state: &mut ClaudeStreamState,
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
                                state.emitted_any_text = true;
                                state.did_work = true;
                                events.publish(SameEvent::MessageDelta {
                                    session_id: session_id.to_string(),
                                    text: text.to_string(),
                                });
                            }
                        }
                        "thinking" => {
                            if let Some(t) = block.get("thinking").and_then(Value::as_str) {
                                state.did_work = true;
                                events.publish(SameEvent::ThinkingDelta {
                                    session_id: session_id.to_string(),
                                    text: t.to_string(),
                                });
                            }
                        }
                        "tool_use" => {
                            state.did_work = true;
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
            if val.get("is_error").and_then(Value::as_bool) == Some(true) {
                state.is_error = true;
                if let Some(errs) = val.get("errors").and_then(Value::as_array) {
                    state
                        .errors
                        .extend(errs.iter().filter_map(Value::as_str).map(str::to_string));
                }
                // A failed run's `result` is the error text, not a reply.
                if state.errors.is_empty() {
                    if let Some(res) = val.get("result").and_then(Value::as_str) {
                        if !res.trim().is_empty() {
                            state.errors.push(res.to_string());
                        }
                    }
                }
                return;
            }
            if !state.emitted_any_text {
                if let Some(res) = val.get("result").and_then(Value::as_str) {
                    if !res.is_empty() {
                        state.emitted_any_text = true;
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
    images: &[String],
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
        // What `--full-auto` did before Codex removed it; a config override
        // also works for `exec resume`, which has no `--sandbox` flag.
        cmd.arg("-c").arg("sandbox_mode=\"workspace-write\"");
    }
    if let Some(m) = model.filter(|s| !s.trim().is_empty()) {
        cmd.arg("-m").arg(m);
    }
    if let Some(e) = effort.filter(|s| !s.trim().is_empty()) {
        cmd.arg("-c").arg(format!("model_reasoning_effort=\"{e}\""));
    }
    // `--image=<path>`: the spaced form takes several values and would
    // swallow the `-` below.
    for img in images {
        cmd.arg(format!("--image={img}"));
    }

    // `-` reads the prompt from stdin, so prompt text starting with `-` is
    // never parsed as an option.
    cmd.arg("-");
    let mut cmd = build_command(target, &provider.command, &cmd.0, cwd, &provider.env).await;
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .with_context(|| format!("Failed to spawn '{}'", provider.command))?;
    let stdin_task = write_prompt_to_stdin(&mut child, prompt)?;

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
    if let Ok(Err(err)) = stdin_task.await {
        return Err(err);
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
    use super::{
        claude_failure_message, handle_claude_json, summarize_tool_call, ClaudeRun,
        ClaudeStreamState,
    };
    use same_core::EventBus;
    use serde_json::json;

    #[test]
    fn claude_failure_message_prefers_result_errors() {
        let msg = claude_failure_message(
            Some(1),
            &["No conversation found with session ID: x".into()],
            &["noise".into()],
        );
        assert_eq!(
            msg,
            "Claude CLI exited with status 1: No conversation found with session ID: x"
        );
        let msg = claude_failure_message(Some(1), &[], &["".into(), "boom".into()]);
        assert_eq!(msg, "Claude CLI exited with status 1: boom");
        assert_eq!(
            claude_failure_message(None, &[], &[]),
            "Claude CLI exited with status -1"
        );
    }

    #[test]
    fn error_result_is_collected_not_shown_as_reply() {
        let events = EventBus::default();
        let mut state = ClaudeStreamState::default();
        let val = serde_json::json!({
            "type": "result",
            "subtype": "error_during_execution",
            "is_error": true,
            "errors": ["No conversation found with session ID: x"],
        });
        handle_claude_json(&val, "s", &events, &mut state);
        assert!(!state.emitted_any_text);
        let run = ClaudeRun::Failed {
            code: Some(1),
            errors: state.errors.clone(),
            stderr_tail: vec![],
            did_work: state.did_work,
        };
        assert!(state.is_error);
        assert!(run.is_missing_conversation("x"));
        assert!(!run.is_missing_conversation("other"));
        let worked = ClaudeRun::Failed {
            code: Some(1),
            errors: state.errors,
            stderr_tail: vec![],
            did_work: true,
        };
        assert!(!worked.is_missing_conversation("x"));
    }

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
