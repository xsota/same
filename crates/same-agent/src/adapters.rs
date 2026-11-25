use crate::permissions::{self, ApprovalPrompt, PermissionGate};
use anyhow::{anyhow, bail, Context, Result};
use same_acp::exec::{build_command, ExecTarget};
use same_core::{EventBus, ProviderConfig, SameEvent, ToolCallRecord, ToolCallStatus};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{oneshot, Mutex};
use tokio::task::JoinSet;

/// Collects a command line so it can be launched in any [`ExecTarget`].
pub(crate) struct ArgList(pub(crate) Vec<String>);

impl ArgList {
    pub(crate) fn new(base: &[String]) -> Self {
        Self(base.to_vec())
    }

    pub(crate) fn arg(&mut self, a: impl Into<String>) -> &mut Self {
        self.0.push(a.into());
        self
    }
}

/// A child's stdin shared by the tasks that answer its requests; `None` once
/// closed.
pub(crate) type SharedStdin = Arc<Mutex<Option<ChildStdin>>>;

pub(crate) fn shared_stdin(child: &mut Child) -> Result<SharedStdin> {
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("Failed to capture agent stdin"))?;
    Ok(Arc::new(Mutex::new(Some(stdin))))
}

/// Writes `val` as one JSON line.
pub(crate) async fn write_json_line(stdin: &SharedStdin, val: &Value) -> Result<()> {
    let mut line = serde_json::to_string(val)?;
    line.push('\n');
    let mut guard = stdin.lock().await;
    let Some(pipe) = guard.as_mut() else {
        bail!("Agent stdin is already closed");
    };
    pipe.write_all(line.as_bytes())
        .await
        .context("Failed to write to agent stdin")?;
    pipe.flush().await.context("Failed to flush agent stdin")?;
    Ok(())
}

/// Closes stdin so the agent sees the end of input.
pub(crate) async fn close_stdin(stdin: &SharedStdin) {
    if let Some(mut pipe) = stdin.lock().await.take() {
        let _ = pipe.shutdown().await;
    }
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
pub(crate) const STDERR_TAIL_LINES: usize = 20;

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

#[allow(clippy::too_many_arguments)]
pub async fn run_claude_cli(
    provider: &ProviderConfig,
    session_id: &str,
    external_session_id: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
    cwd: &Path,
    target: &ExecTarget,
    prompt: &str,
    gate: PermissionGate,
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
            &gate,
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

/// Runs one turn with `stream-json` on both stdin and stdout. Permission
/// prompts arrive as `can_use_tool` control requests (through
/// `--permission-prompt-tool stdio`) and are answered on stdin, so stdin stays
/// open until the turn's `result` line.
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
    gate: &PermissionGate,
    events: &EventBus,
    cancel_rx: &mut oneshot::Receiver<()>,
) -> Result<ClaudeRun> {
    let mut cmd = ArgList::new(&provider.args);
    cmd.arg("-p")
        .arg("--input-format")
        .arg("stream-json")
        .arg("--output-format")
        .arg("stream-json")
        .arg("--verbose")
        .arg("--permission-prompt-tool")
        .arg("stdio");

    if let Some(m) = model.filter(|s| !s.trim().is_empty()) {
        cmd.arg("--model").arg(m);
    }
    if let Some(e) = effort.filter(|s| !s.trim().is_empty()) {
        cmd.arg("--effort").arg(e);
    }
    if let Some(ext) = resume {
        cmd.arg("--resume").arg(ext);
    }

    let mut cmd = build_command(target, &provider.command, &cmd.0, cwd, &provider.env).await;
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .with_context(|| format!("Failed to spawn '{}'", provider.command))?;
    let stdin = shared_stdin(&mut child)?;
    // Written in the background: a large prompt must not block reading the
    // child's output.
    let prompt_task = {
        let stdin = stdin.clone();
        let message = claude_user_message(prompt);
        tokio::spawn(async move { write_json_line(&stdin, &message).await })
    };

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
    // Open permission prompts, by control request id.
    let mut prompts: HashMap<String, tokio::task::AbortHandle> = HashMap::new();
    let mut prompt_tasks: JoinSet<(Option<String>, Result<()>)> = JoinSet::new();

    loop {
        tokio::select! {
            Some(res) = prompt_tasks.join_next(), if !prompt_tasks.is_empty() => {
                if let Ok((rid, write_res)) = res {
                    if let Some(rid) = rid {
                        prompts.remove(&rid);
                    }
                    if let Err(err) = write_res {
                        prompt_task.abort();
                        prompt_tasks.abort_all();
                        let _ = child.kill().await;
                        close_stdin(&stdin).await;
                        return Err(err);
                    }
                }
            }
            _ = &mut *cancel_rx => {
                prompt_task.abort();
                prompt_tasks.abort_all();
                let _ = child.kill().await;
                close_stdin(&stdin).await;
                return Ok(ClaudeRun::Cancelled);
            }
            line_res = lines.next_line() => {
                match line_res? {
                    Some(line) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        let Ok(val) = serde_json::from_str::<Value>(trimmed) else {
                            events.publish(SameEvent::AgentLog {
                                session_id: Some(session_id.to_string()),
                                provider_id: provider.id.clone(),
                                line: trimmed.to_string(),
                            });
                            continue;
                        };
                        match val.get("type").and_then(Value::as_str).unwrap_or("") {
                            "control_request" => {
                                if let Some((rid, handle)) = spawn_claude_control_request(
                                    &val,
                                    session_id,
                                    gate,
                                    &stdin,
                                    &state.denied,
                                    &mut prompt_tasks,
                                ) {
                                    prompts.insert(rid, handle);
                                }
                            }
                            "control_cancel_request" => {
                                // Claude no longer needs the answer (e.g. the
                                // turn was interrupted).
                                if let Some(handle) = val
                                    .get("request_id")
                                    .and_then(Value::as_str)
                                    .and_then(|rid| prompts.remove(rid))
                                {
                                    handle.abort();
                                }
                            }
                            "control_response" => {}
                            msg_type => {
                                handle_claude_json(&val, session_id, events, &mut state);
                                if msg_type == "result" {
                                    // The turn is over; closing stdin lets
                                    // the CLI exit.
                                    prompt_tasks.abort_all();
                                    close_stdin(&stdin).await;
                                }
                            }
                        }
                    }
                    None => break,
                }
            }
        }
    }
    prompt_tasks.abort_all();
    close_stdin(&stdin).await;

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
    if let Ok(Err(err)) = prompt_task.await {
        return Err(err);
    }
    Ok(ClaudeRun::Completed)
}

/// A `stream-json` user message carrying the prompt.
fn claude_user_message(prompt: &str) -> Value {
    json!({
        "type": "user",
        "message": {"role": "user", "content": prompt},
        "parent_tool_use_id": null,
        "session_id": "",
    })
}

/// Text Claude sees when the user denies a tool.
const CLAUDE_DENY_MESSAGE: &str = "The user denied this action.";

/// Answers a control request from Claude Code. `can_use_tool` asks the user
/// in a background task (returned so it can be aborted); anything else gets
/// an error response right away.
fn spawn_claude_control_request(
    val: &Value,
    session_id: &str,
    gate: &PermissionGate,
    stdin: &SharedStdin,
    denied: &DeniedTools,
    tasks: &mut JoinSet<(Option<String>, Result<()>)>,
) -> Option<(String, tokio::task::AbortHandle)> {
    let request_id = val.get("request_id").and_then(Value::as_str)?.to_string();
    let request = val.get("request").cloned().unwrap_or(Value::Null);
    let stdin = stdin.clone();
    let subtype = request
        .get("subtype")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if subtype != "can_use_tool" {
        tasks.spawn(async move {
            let reply = claude_control_error(
                &request_id,
                &format!("Unsupported control request: {subtype}"),
            );
            let res = tokio::spawn(async move { write_json_line(&stdin, &reply).await })
                .await
                .unwrap_or(Ok(()));
            (None, res)
        });
        return None;
    }
    if request.get("tool_name").and_then(Value::as_str) == Some("AskUserQuestion") {
        let rid = request_id.clone();
        tasks.spawn(async move {
            let reply = json!({
                "type": "control_response",
                "response": {
                    "subtype": "success",
                    "request_id": rid,
                    "response": {
                        "behavior": "deny",
                        "message": "AskUserQuestion is not supported in SAME; ask the user directly in your reply.",
                    },
                },
            });
            let res = tokio::spawn(async move { write_json_line(&stdin, &reply).await })
                .await
                .unwrap_or(Ok(()));
            (None, res)
        });
        return None;
    }

    let gate = gate.clone();
    let sid = session_id.to_string();
    let denied = denied.clone();
    let rid = request_id.clone();
    let handle = tasks.spawn(async move {
        let choice = gate.ask(&sid, claude_permission_prompt(&request)).await;
        let reply = claude_permission_response(&rid, &request, choice.as_deref());
        if reply
            .pointer("/response/response/behavior")
            .and_then(Value::as_str)
            == Some("deny")
        {
            if let Some(id) = request.get("tool_use_id").and_then(Value::as_str) {
                denied.lock().unwrap().insert(id.to_string());
            }
        }
        // Detach the write so a late `control_cancel_request` abort cannot
        // truncate a JSON line mid-frame.
        let res = tokio::spawn(async move { write_json_line(&stdin, &reply).await })
            .await
            .unwrap_or(Ok(()));
        (Some(rid), res)
    });
    Some((request_id, handle))
}

/// The approval prompt for a `can_use_tool` request.
fn claude_permission_prompt(request: &Value) -> ApprovalPrompt {
    let tool = request
        .get("display_name")
        .or_else(|| request.get("tool_name"))
        .and_then(Value::as_str)
        .unwrap_or("Tool");
    let input = request.get("input");
    let mut description = String::new();
    if let Some(d) = request
        .get("description")
        .and_then(Value::as_str)
        .filter(|d| !d.trim().is_empty())
    {
        description.push_str(d.trim());
        description.push_str("\n\n");
    }
    if let Some(i) = input {
        description.push_str(&serde_json::to_string_pretty(i).unwrap_or_default());
    }
    let mut options = vec![permissions::option("allow", "Allow", "allow_once")];
    let rule_suggestions = claude_rule_suggestions(request);
    if !rule_suggestions.is_empty() {
        let summary = format_claude_rule_suggestions(&rule_suggestions);
        if !summary.is_empty() {
            if !description.is_empty() {
                description.push_str("\n\n");
            }
            description.push_str("Always allow rules:\n");
            description.push_str(&summary);
        }
        options.push(permissions::option(
            "allow_always",
            "Always allow",
            "allow_always",
        ));
    }
    options.push(permissions::option("deny", "Deny", "reject_once"));
    ApprovalPrompt {
        tool_call_id: request
            .get("tool_use_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        title: summarize_tool_call(tool, input),
        description,
        options,
    }
}

/// The `addRules` permission updates Claude suggests for "don't ask again".
/// Other suggestions (extra directories, switching mode) are left out so
/// "Always allow" grants only this kind of call.
fn claude_rule_suggestions(request: &Value) -> Vec<Value> {
    request
        .get("permission_suggestions")
        .and_then(Value::as_array)
        .map(|s| {
            s.iter()
                .filter(|u| u.get("type").and_then(Value::as_str) == Some("addRules"))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

fn format_claude_rule_suggestions(suggestions: &[Value]) -> String {
    let mut lines = Vec::new();
    for suggestion in suggestions {
        let dest = suggestion
            .get("destination")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        if let Some(rules) = suggestion.get("rules").and_then(Value::as_array) {
            for rule in rules {
                let tool = rule
                    .get("toolName")
                    .and_then(Value::as_str)
                    .unwrap_or("Tool");
                let content = rule
                    .get("ruleContent")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty());
                let spec = match content {
                    Some(c) => format!("{tool}({c})"),
                    None => tool.to_string(),
                };
                match dest {
                    Some(d) => lines.push(format!("- {spec} ({d})")),
                    None => lines.push(format!("- {spec}")),
                }
            }
        }
    }
    lines.join("\n")
}

/// The control response for a `can_use_tool` request. `choice` is the
/// selected option id; `None` (stopped or dismissed) denies and interrupts.
fn claude_permission_response(request_id: &str, request: &Value, choice: Option<&str>) -> Value {
    let input = request.get("input").cloned().unwrap_or_else(|| json!({}));
    let decision = match choice {
        Some("allow") => json!({"behavior": "allow", "updatedInput": input}),
        Some("allow_always") => json!({
            "behavior": "allow",
            "updatedInput": input,
            "updatedPermissions": claude_rule_suggestions(request),
        }),
        Some(_) => json!({"behavior": "deny", "message": CLAUDE_DENY_MESSAGE}),
        None => json!({
            "behavior": "deny",
            "message": CLAUDE_DENY_MESSAGE,
            "interrupt": true,
        }),
    };
    json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": decision,
        },
    })
}

fn claude_control_error(request_id: &str, message: &str) -> Value {
    json!({
        "type": "control_response",
        "response": {
            "subtype": "error",
            "request_id": request_id,
            "error": message,
        },
    })
}

/// What a `stream-json` run has reported so far.
#[derive(Default)]
struct ClaudeStreamState {
    emitted_any_text: bool,
    did_work: bool,
    is_error: bool,
    /// `errors` of an `is_error` result line.
    errors: Vec<String>,
    /// Tool uses the user denied, so their error results show as denied.
    denied: DeniedTools,
}

type DeniedTools = Arc<std::sync::Mutex<HashSet<String>>>;

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
                        let status = if is_err && state.denied.lock().unwrap().contains(&id) {
                            ToolCallStatus::Denied
                        } else if is_err {
                            ToolCallStatus::Failed
                        } else {
                            ToolCallStatus::Completed
                        };
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
                                status,
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
        claude_failure_message, claude_permission_prompt, claude_permission_response,
        handle_claude_json, summarize_tool_call, ClaudeRun, ClaudeStreamState,
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
    fn claude_permission_prompt_and_responses() {
        let request = json!({
            "subtype": "can_use_tool",
            "tool_name": "Bash",
            "display_name": "Bash",
            "input": {"command": "touch a.txt", "description": "Create a.txt"},
            "description": "Create a.txt",
            "permission_suggestions": [
                {"type": "addRules", "rules": [{"toolName": "Bash", "ruleContent": "touch a.txt"}],
                 "behavior": "allow", "destination": "localSettings"},
                {"type": "setMode", "mode": "acceptEdits", "destination": "session"}
            ],
            "tool_use_id": "toolu_1"
        });
        let prompt = claude_permission_prompt(&request);
        assert_eq!(prompt.title, "Bash touch a.txt");
        assert_eq!(prompt.tool_call_id.as_deref(), Some("toolu_1"));
        let kinds: Vec<&str> = prompt.options.iter().map(|o| o.kind.as_str()).collect();
        assert_eq!(kinds, ["allow_once", "allow_always", "reject_once"]);

        let allow = claude_permission_response("r1", &request, Some("allow"));
        assert_eq!(allow["type"], "control_response");
        assert_eq!(allow["response"]["request_id"], "r1");
        assert_eq!(allow["response"]["response"]["behavior"], "allow");
        assert_eq!(
            allow["response"]["response"]["updatedInput"]["command"],
            "touch a.txt"
        );

        let always = claude_permission_response("r1", &request, Some("allow_always"));
        let updates = always["response"]["response"]["updatedPermissions"]
            .as_array()
            .unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0]["type"], "addRules");

        let deny = claude_permission_response("r1", &request, Some("deny"));
        assert_eq!(deny["response"]["response"]["behavior"], "deny");
        assert!(deny["response"]["response"].get("interrupt").is_none());
        let stopped = claude_permission_response("r1", &request, None);
        assert_eq!(stopped["response"]["response"]["interrupt"], true);

        let no_rules = json!({"tool_name": "Edit", "input": {"file_path": "x"}});
        let kinds: Vec<String> = claude_permission_prompt(&no_rules)
            .options
            .into_iter()
            .map(|o| o.kind)
            .collect();
        assert_eq!(kinds, ["allow_once", "reject_once"]);
    }

    #[test]
    fn denied_tool_result_shows_as_denied() {
        let events = EventBus::default();
        let mut rx = events.subscribe();
        let mut state = ClaudeStreamState::default();
        state.denied.lock().unwrap().insert("toolu_1".into());
        let val = json!({"type": "user", "message": {"content": [
            {"type": "tool_result", "tool_use_id": "toolu_1", "is_error": true, "content": "denied"},
            {"type": "tool_result", "tool_use_id": "toolu_2", "is_error": true, "content": "boom"}
        ]}});
        handle_claude_json(&val, "s", &events, &mut state);
        let mut statuses = vec![];
        while let Ok(same_core::SameEvent::ToolCallUpdated { tool_call, .. }) = rx.try_recv() {
            statuses.push(tool_call.status);
        }
        assert_eq!(
            statuses,
            [
                same_core::ToolCallStatus::Denied,
                same_core::ToolCallStatus::Failed
            ]
        );
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
