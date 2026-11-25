//! Runs a Codex turn through `codex app-server` (JSON-RPC over stdio), which
//! sends command and file-change approval requests the user can answer.

use crate::adapters::{
    close_stdin, shared_stdin, write_json_line, ArgList, SharedStdin, STDERR_TAIL_LINES,
};
use crate::permissions::{self, ApprovalPrompt, PermissionGate};
use anyhow::{anyhow, Context, Result};
use same_acp::exec::{build_command, ExecTarget};
use same_core::{EventBus, ProviderConfig, SameEvent, ToolCallRecord, ToolCallStatus};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::oneshot;
use tokio::task::JoinSet;

const ID_INITIALIZE: u64 = 1;
const ID_THREAD: u64 = 2;
const ID_TURN: u64 = 3;
/// `thread/start` after the stored thread could not be resumed.
const ID_THREAD_FRESH: u64 = 4;
const ID_INTERRUPT: u64 = 5;

/// How the turn ended, from `turn/completed`.
#[derive(Debug, PartialEq)]
enum TurnEnd {
    Completed,
    Interrupted,
    Failed(String),
}

#[derive(Default)]
struct CodexTurn {
    thread_id: Option<String>,
    turn_id: Option<String>,
    /// Agent message / reasoning items whose text arrived as deltas.
    streamed: HashSet<String>,
    emitted_text: bool,
    /// Items seen so far, by id, to describe approval requests.
    items: HashMap<String, Value>,
    /// Last non-retrying `error` notification.
    error: Option<String>,
}

#[allow(clippy::too_many_arguments)]
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
    gate: PermissionGate,
    events: EventBus,
    mut cancel_rx: oneshot::Receiver<()>,
) -> Result<()> {
    let mut args = ArgList::new(&provider.args);
    args.arg("app-server");
    let mut cmd = build_command(target, &provider.command, &args.0, cwd, &provider.env).await;
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd
        .spawn()
        .with_context(|| format!("Failed to spawn '{}'", provider.command))?;
    let stdin = shared_stdin(&mut child)?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("Failed to capture Codex stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("Failed to capture Codex stderr"))?;

    let stderr_tail = Arc::new(std::sync::Mutex::new(VecDeque::new()));
    {
        let events = events.clone();
        let sid = session_id.to_string();
        let pid = provider.id.clone();
        let tail = stderr_tail.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                {
                    let mut tail = tail.lock().unwrap();
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
        });
    }

    let agent_cwd = target.to_target_path(cwd);
    // Always use `untrusted` so the server emits approval requests and
    // `PermissionGate` can apply the live `auto_approve` toggle mid-turn.
    let approval_policy = "untrusted";
    let resume = external_session_id.filter(|s| !s.trim().is_empty());
    let thread_request = match resume {
        Some(thread_id) => {
            let mut params = json!({
                "threadId": thread_id,
                "cwd": agent_cwd,
                "approvalPolicy": approval_policy,
            });
            if auto_approve {
                params["sandbox"] = json!("workspace-write");
            }
            rpc_request(ID_THREAD, "thread/resume", params)
        }
        None => thread_start(ID_THREAD, &agent_cwd, approval_policy, auto_approve),
    };
    for msg in [
        rpc_request(
            ID_INITIALIZE,
            "initialize",
            json!({"clientInfo": {"name": "same", "version": env!("CARGO_PKG_VERSION")}}),
        ),
        json!({"jsonrpc": "2.0", "method": "initialized"}),
        thread_request,
    ] {
        write_json_line(&stdin, &msg).await?;
    }

    let mut lines = BufReader::new(stdout).lines();
    let mut turn = CodexTurn::default();
    let mut request_tasks: JoinSet<Result<()>> = JoinSet::new();
    let mut pending_requests: HashMap<String, tokio::task::AbortHandle> = HashMap::new();

    let end: Option<TurnEnd> = loop {
        tokio::select! {
            Some(res) = request_tasks.join_next(), if !request_tasks.is_empty() => {
                if let Ok(Err(err)) = res {
                    break Some(TurnEnd::Failed(format!("Codex stdin write failed: {err}")));
                }
            }
            _ = &mut cancel_rx => {
                request_tasks.abort_all();
                let graceful = async {
                    if let (Some(thread_id), Some(turn_id)) = (&turn.thread_id, &turn.turn_id) {
                        let _ = write_json_line(
                            &stdin,
                            &rpc_request(
                                ID_INTERRUPT,
                                "turn/interrupt",
                                json!({"threadId": thread_id, "turnId": turn_id}),
                            ),
                        )
                        .await;
                    }
                    close_stdin(&stdin).await;
                    let _ = child.wait().await;
                };
                if tokio::time::timeout(Duration::from_secs(2), graceful).await.is_err() {
                    let _ = child.kill().await;
                }
                events.publish(SameEvent::SessionCancelled {
                    session_id: session_id.to_string(),
                });
                return Ok(());
            }
            line_res = lines.next_line() => {
                let Some(line) = line_res? else { break None };
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
                // Server requests have their own ids, which can collide with
                // ours: route on `method` first.
                let method = val.get("method").and_then(Value::as_str);
                let has_id = val.get("id").is_some_and(|id| !id.is_null());
                match (method, has_id) {
                    (Some(method), true) => {
                        if let Some((key, handle)) = spawn_server_request(
                            method,
                            &val,
                            &turn,
                            session_id,
                            &gate,
                            &stdin,
                            &mut request_tasks,
                        ) {
                            pending_requests.insert(key, handle);
                        }
                    }
                    (Some("serverRequest/resolved"), false) => {
                        if let Some(key) = val
                            .pointer("/params/requestId")
                            .filter(|v| !v.is_null())
                            .map(Value::to_string)
                        {
                            if let Some(handle) = pending_requests.remove(&key) {
                                handle.abort();
                            }
                        }
                    }
                    (Some(method), false) => {
                        if let Some(end) =
                            handle_notification(method, &val, session_id, &events, &mut turn)
                        {
                            break Some(end);
                        }
                    }
                    (None, true) => {
                        let id = val.get("id").and_then(Value::as_u64).unwrap_or(0);
                        let error = val
                            .get("error")
                            .map(|e| {
                                e.get("message")
                                    .and_then(Value::as_str)
                                    .unwrap_or("Codex request failed")
                                    .to_string()
                            });
                        match (id, error) {
                            (ID_THREAD, Some(err))
                                if resume.is_some() && err.contains("no rollout found") =>
                            {
                                events.publish(SameEvent::AgentLog {
                                    session_id: Some(session_id.to_string()),
                                    provider_id: provider.id.clone(),
                                    line: format!(
                                        "Codex thread {} not found; starting a new thread",
                                        resume.unwrap_or_default()
                                    ),
                                });
                                write_json_line(
                                    &stdin,
                                    &thread_start(
                                        ID_THREAD_FRESH,
                                        &agent_cwd,
                                        approval_policy,
                                        auto_approve,
                                    ),
                                )
                                .await?;
                            }
                            (ID_INTERRUPT, _) => {}
                            (_, Some(err)) => break Some(TurnEnd::Failed(err)),
                            (ID_THREAD | ID_THREAD_FRESH, None) => {
                                let Some(thread_id) = val
                                    .pointer("/result/thread/id")
                                    .and_then(Value::as_str)
                                else {
                                    break Some(TurnEnd::Failed(
                                        "Codex did not return a thread id".to_string(),
                                    ));
                                };
                                turn.thread_id = Some(thread_id.to_string());
                                events.publish(SameEvent::ExternalSessionBound {
                                    session_id: session_id.to_string(),
                                    external_session_id: thread_id.to_string(),
                                });
                                let stdin = stdin.clone();
                                let turn_req =
                                    turn_start(thread_id, prompt, images, model, effort);
                                request_tasks.spawn(async move {
                                    write_json_line(&stdin, &turn_req).await
                                });
                            }
                            (ID_TURN, None) => {
                                if let Some(id) =
                                    val.pointer("/result/turn/id").and_then(Value::as_str)
                                {
                                    turn.turn_id = Some(id.to_string());
                                }
                            }
                            _ => {}
                        }
                    }
                    (None, false) => {}
                }
            }
        }
    };

    request_tasks.abort_all();
    close_stdin(&stdin).await;
    let status = match tokio::time::timeout(Duration::from_secs(3), child.wait()).await {
        Ok(status) => status.ok(),
        Err(_) => {
            let _ = child.kill().await;
            None
        }
    };

    match end {
        Some(TurnEnd::Completed) => {
            events.publish(SameEvent::SessionCompleted {
                session_id: session_id.to_string(),
                stop_reason: Some("end_turn".to_string()),
            });
            Ok(())
        }
        // e.g. the user chose "Deny and stop".
        Some(TurnEnd::Interrupted) => {
            events.publish(SameEvent::SessionCancelled {
                session_id: session_id.to_string(),
            });
            Ok(())
        }
        Some(TurnEnd::Failed(err)) => Err(anyhow!(err)),
        None => {
            let tail: Vec<String> = stderr_tail.lock().unwrap().iter().cloned().collect();
            let mut msg = format!(
                "Codex app-server exited with status {} before the turn finished",
                status.and_then(|s| s.code()).unwrap_or(-1)
            );
            if let Some(err) = turn.error {
                msg.push_str(": ");
                msg.push_str(&err);
            } else if let Some(last) = tail.iter().rev().find(|l| !l.trim().is_empty()) {
                msg.push_str(": ");
                msg.push_str(last.trim());
            }
            Err(anyhow!(msg))
        }
    }
}

fn rpc_request(id: u64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

fn thread_start(id: u64, cwd: &str, approval_policy: &str, auto_approve: bool) -> Value {
    let mut params = json!({
        "cwd": cwd,
        "approvalPolicy": approval_policy,
    });
    if auto_approve {
        params["sandbox"] = json!("workspace-write");
    }
    rpc_request(id, "thread/start", params)
}

fn turn_start(
    thread_id: &str,
    prompt: &str,
    images: &[String],
    model: Option<&str>,
    effort: Option<&str>,
) -> Value {
    let mut input = vec![json!({"type": "text", "text": prompt})];
    input.extend(
        images
            .iter()
            .map(|path| json!({"type": "localImage", "path": path})),
    );
    let mut params = json!({"threadId": thread_id, "input": input});
    if let Some(m) = model.filter(|s| !s.trim().is_empty()) {
        params["model"] = json!(m);
    }
    if let Some(e) = effort.filter(|s| !s.trim().is_empty()) {
        params["effort"] = json!(e);
    }
    rpc_request(ID_TURN, "turn/start", params)
}

/// Handles a notification; returns how the turn ended once it has.
fn handle_notification(
    method: &str,
    val: &Value,
    session_id: &str,
    events: &EventBus,
    turn: &mut CodexTurn,
) -> Option<TurnEnd> {
    let params = val.get("params").cloned().unwrap_or(Value::Null);
    // Keep approval-relevant items cached across sub-agent threads too, since
    // server approval requests can arrive for sub-agent edits/commands.
    if matches!(method, "item/started" | "item/completed") {
        if let Some(item) = params.get("item") {
            let id = item.get("id").and_then(Value::as_str).unwrap_or("item");
            let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
            if method == "item/completed" {
                turn.items.remove(id);
            } else if matches!(item_type, "commandExecution" | "fileChange") {
                turn.items.insert(id.to_string(), item.clone());
            }
        }
    }
    // Sub-agent threads report on the same connection; only this thread's
    // events belong to the session.
    if let (Some(ours), Some(theirs)) = (
        turn.thread_id.as_deref(),
        params.get("threadId").and_then(Value::as_str),
    ) {
        if ours != theirs {
            return None;
        }
    }
    let str_param = |k: &str| params.get(k).and_then(Value::as_str).unwrap_or("");
    match method {
        "turn/started" => {
            if let Some(id) = params.pointer("/turn/id").and_then(Value::as_str) {
                turn.turn_id = Some(id.to_string());
            }
        }
        "item/agentMessage/delta" => {
            let delta = str_param("delta");
            if !delta.is_empty() {
                let item_id = str_param("itemId").to_string();
                let first = turn.streamed.insert(item_id);
                publish_text(events, session_id, turn, delta, first);
            }
        }
        "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => {
            let delta = str_param("delta");
            if !delta.is_empty() {
                turn.streamed.insert(str_param("itemId").to_string());
                events.publish(SameEvent::ThinkingDelta {
                    session_id: session_id.to_string(),
                    text: delta.to_string(),
                });
            }
        }
        "item/started" | "item/completed" => {
            let item = params.get("item")?;
            let id = item.get("id").and_then(Value::as_str).unwrap_or("item");
            let completed = method == "item/completed";
            let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
            match item_type {
                "agentMessage" if completed && !turn.streamed.contains(id) => {
                    if let Some(text) = item.get("text").and_then(Value::as_str) {
                        if !text.is_empty() {
                            publish_text(events, session_id, turn, text, true);
                        }
                    }
                }
                "reasoning" if completed && !turn.streamed.contains(id) => {
                    let text = reasoning_text(item);
                    if !text.is_empty() {
                        events.publish(SameEvent::ThinkingDelta {
                            session_id: session_id.to_string(),
                            text,
                        });
                    }
                }
                _ => {
                    if let Some(tool_call) = tool_record(item, completed) {
                        let session_id = session_id.to_string();
                        events.publish(if completed {
                            SameEvent::ToolCallUpdated {
                                session_id,
                                tool_call,
                            }
                        } else {
                            SameEvent::ToolCallStarted {
                                session_id,
                                tool_call,
                            }
                        });
                    }
                }
            }
        }
        "error" => {
            if params.get("willRetry").and_then(Value::as_bool) != Some(true) {
                if let Some(msg) = params.pointer("/error/message").and_then(Value::as_str) {
                    turn.error = Some(msg.to_string());
                }
            }
        }
        "turn/completed" => {
            let t = params.get("turn")?;
            return Some(match t.get("status").and_then(Value::as_str) {
                Some("interrupted") => TurnEnd::Interrupted,
                Some("failed") => TurnEnd::Failed(
                    t.pointer("/error/message")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .or_else(|| turn.error.clone())
                        .unwrap_or_else(|| "Codex turn failed".to_string()),
                ),
                _ => TurnEnd::Completed,
            });
        }
        _ => {}
    }
    None
}

/// Publishes reply text; a new message is separated from earlier ones.
fn publish_text(
    events: &EventBus,
    session_id: &str,
    turn: &mut CodexTurn,
    text: &str,
    new_message: bool,
) {
    let text = if new_message && turn.emitted_text {
        format!("\n\n{text}")
    } else {
        text.to_string()
    };
    turn.emitted_text = true;
    events.publish(SameEvent::MessageDelta {
        session_id: session_id.to_string(),
        text,
    });
}

fn reasoning_text(item: &Value) -> String {
    let parts = |k: &str| -> Vec<String> {
        item.get(k)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|p| {
                        p.as_str()
                            .or_else(|| p.get("text").and_then(Value::as_str))
                            .map(str::to_string)
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let summary = parts("summary");
    if summary.is_empty() {
        parts("content").join("\n\n")
    } else {
        summary.join("\n\n")
    }
}

/// The command as the user would type it: the parsed actions when Codex has
/// Unwraps a single parsed command action (removing the `zsh -lc` wrapper),
/// but keeps the raw command when there are multiple actions so shell
/// operators (`|`, `;`, `||`, `&&`) are never misrepresented.
fn command_display(item: &Value) -> String {
    let actions: Vec<&str> = item
        .get("commandActions")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.get("command").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    if actions.len() == 1 {
        return actions[0].to_string();
    }
    if let Some(raw) = item
        .get("command")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
    {
        return raw.to_string();
    }
    if actions.is_empty() {
        "Command".to_string()
    } else {
        actions.join("; ")
    }
}

fn changed_paths(item: &Value) -> Vec<String> {
    item.get("changes")
        .and_then(Value::as_array)
        .map(|c| {
            c.iter()
                .filter_map(|x| x.get("path").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn changes_diff(item: &Value) -> String {
    item.get("changes")
        .and_then(Value::as_array)
        .map(|c| {
            c.iter()
                .map(|x| {
                    format!(
                        "{}\n{}",
                        x.get("path").and_then(Value::as_str).unwrap_or(""),
                        x.get("diff").and_then(Value::as_str).unwrap_or("")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn item_status(item: &Value, completed: bool) -> ToolCallStatus {
    match item.get("status").and_then(Value::as_str) {
        Some("declined") => ToolCallStatus::Denied,
        Some("failed") => ToolCallStatus::Failed,
        Some("completed") => {
            if item
                .get("exitCode")
                .and_then(Value::as_i64)
                .is_some_and(|c| c != 0)
            {
                ToolCallStatus::Failed
            } else {
                ToolCallStatus::Completed
            }
        }
        Some("inProgress") if !completed => ToolCallStatus::InProgress,
        _ if completed => ToolCallStatus::Completed,
        _ => ToolCallStatus::InProgress,
    }
}

/// The tool card for an item, if it is a tool use.
fn tool_record(item: &Value, completed: bool) -> Option<ToolCallRecord> {
    let id = item.get("id").and_then(Value::as_str)?.to_string();
    let status = item_status(item, completed);
    let (title, kind, input, output) = match item.get("type").and_then(Value::as_str)? {
        "commandExecution" => {
            let command = command_display(item);
            (
                format!("Shell: {command}"),
                "command_execution",
                Some(command),
                item.get("aggregatedOutput")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            )
        }
        "fileChange" => (
            format!("Edit {}", changed_paths(item).join(", ")),
            "file_change",
            None,
            Some(changes_diff(item)),
        ),
        "mcpToolCall" => (
            format!(
                "{}.{}",
                item.get("server").and_then(Value::as_str).unwrap_or("mcp"),
                item.get("tool").and_then(Value::as_str).unwrap_or("tool")
            ),
            "mcp_tool_call",
            item.get("arguments")
                .map(|a| serde_json::to_string_pretty(a).unwrap_or_default()),
            item.get("error")
                .filter(|e| !e.is_null())
                .or_else(|| item.get("result").filter(|r| !r.is_null()))
                .map(|r| serde_json::to_string_pretty(r).unwrap_or_default()),
        ),
        "webSearch" => (
            format!(
                "Web search: {}",
                item.get("query").and_then(Value::as_str).unwrap_or("")
            ),
            "web_search",
            None,
            None,
        ),
        _ => return None,
    };
    Some(ToolCallRecord {
        id,
        title,
        kind: Some(kind.to_string()),
        status,
        input,
        output,
    })
}

/// An approval prompt and, per option (by index), the decision sent back.
struct CodexApproval {
    prompt: ApprovalPrompt,
    decisions: Vec<Value>,
}

/// Builds the prompt for an approval request, or `None` if the method is
/// not one SAME can answer.
fn codex_approval(
    method: &str,
    params: &Value,
    items: &HashMap<String, Value>,
) -> Option<CodexApproval> {
    let item_id = params.get("itemId").and_then(Value::as_str);
    let item = item_id.and_then(|id| items.get(id));
    let reason = params
        .get("reason")
        .and_then(Value::as_str)
        .filter(|r| !r.trim().is_empty());
    let (title, mut description) = match method {
        "item/commandExecution/requestApproval" => {
            let raw_command = params
                .get("command")
                .or_else(|| item.and_then(|i| i.get("command")))
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty());
            let display = item
                .map(command_display)
                .unwrap_or_else(|| command_display(params));
            let command = raw_command.unwrap_or(&display);
            let mut desc = format!("$ {command}");
            if let Some(cwd) = params.get("cwd").and_then(Value::as_str) {
                desc.push_str(&format!("\n\ncwd: {cwd}"));
            }
            if let Some(host) = params
                .pointer("/networkApprovalContext/host")
                .and_then(Value::as_str)
            {
                desc.push_str(&format!("\nnetwork: {host}"));
            }
            if let Some(extra) = params
                .get("additionalPermissions")
                .filter(|v| !v.is_null() && v.as_object().is_none_or(|m| !m.is_empty()))
            {
                if let Ok(pretty) = serde_json::to_string_pretty(extra) {
                    desc.push_str(&format!("\n\nadditional permissions:\n{pretty}"));
                }
            }
            (format!("Run: {display}"), desc)
        }
        "item/fileChange/requestApproval" => {
            let paths = item.map(changed_paths).unwrap_or_default();
            let title = if paths.is_empty() {
                "Edit files".to_string()
            } else {
                format!("Edit {}", paths.join(", "))
            };
            let mut desc = String::new();
            if let Some(root) = params
                .get("grantRoot")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
            {
                desc.push_str(&format!("Grant write access to directory: {root}"));
            }
            let diff = item.map(changes_diff).unwrap_or_default();
            if !diff.is_empty() {
                if !desc.is_empty() {
                    desc.push_str("\n\n");
                }
                desc.push_str(&diff);
            }
            (title, desc)
        }
        _ => return None,
    };
    if let Some(r) = reason {
        description = format!("{r}\n\n{description}");
    }

    let available: Vec<Value> = params
        .get("availableDecisions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_else(|| vec![json!("accept"), json!("decline"), json!("cancel")]);
    let mut decisions = Vec::new();
    let mut options = Vec::new();
    let mut push = |decision: Value, name: String, kind: &str| {
        options.push(permissions::option(
            &decisions.len().to_string(),
            name,
            kind,
        ));
        decisions.push(decision);
    };
    let has_accept = available.iter().any(|d| d.as_str() == Some("accept"));
    for d in &available {
        match d.as_str() {
            Some("accept") => push(d.clone(), "Allow".into(), "allow_once"),
            // The app-server only lives for one turn, so only expose
            // `acceptForSession` when `accept` is not offered (e.g. session-
            // scoped network approvals). Keep it `allow_always` so single-use
            // auto-approve does not grant a turn-wide permission.
            Some("acceptForSession") if !has_accept => {
                push(d.clone(), "Allow for this turn".into(), "allow_always")
            }
            Some(_) => {}
            None => {
                if let Some(rule) = d
                    .pointer("/acceptWithExecpolicyAmendment/execpolicy_amendment")
                    .and_then(Value::as_array)
                {
                    let words: Vec<&str> = rule.iter().filter_map(Value::as_str).collect();
                    push(
                        d.clone(),
                        format!("Always allow `{}`", words.join(" ")),
                        "allow_always",
                    );
                } else if let Some(net) =
                    d.pointer("/applyNetworkPolicyAmendment/network_policy_amendment")
                {
                    let action = net.get("action").and_then(Value::as_str).unwrap_or("");
                    let host = net.get("host").and_then(Value::as_str).unwrap_or("host");
                    match action {
                        "allow" => push(
                            d.clone(),
                            format!("Always allow network {host}"),
                            "allow_always",
                        ),
                        "deny" => push(
                            d.clone(),
                            format!("Always deny network {host}"),
                            "reject_always",
                        ),
                        _ => {}
                    }
                }
            }
        }
    }
    // `decline` works even when it is not listed: deny and let the turn go on.
    push(json!("decline"), "Deny".into(), "reject_once");
    if available.iter().any(|d| d.as_str() == Some("cancel")) {
        push(json!("cancel"), "Deny and stop".into(), "reject_always");
    }

    Some(CodexApproval {
        prompt: ApprovalPrompt {
            tool_call_id: item_id.map(str::to_string),
            title,
            description,
            options,
        },
        decisions,
    })
}

/// The JSON-RPC response for an approval: the chosen decision, or `cancel`
/// when the prompt was dismissed (the run was stopped).
fn approval_response(id: &Value, decisions: &[Value], choice: Option<&str>) -> Value {
    let decision = choice
        .and_then(|c| c.parse::<usize>().ok())
        .and_then(|i| decisions.get(i).cloned())
        .unwrap_or_else(|| json!("cancel"));
    json!({"jsonrpc": "2.0", "id": id, "result": {"decision": decision}})
}

/// Answers a server request: approvals go to the user in a background task;
/// anything else is refused so the turn does not wait forever.
fn spawn_server_request(
    method: &str,
    val: &Value,
    turn: &CodexTurn,
    session_id: &str,
    gate: &PermissionGate,
    stdin: &SharedStdin,
    tasks: &mut JoinSet<Result<()>>,
) -> Option<(String, tokio::task::AbortHandle)> {
    let id = val.get("id").cloned().unwrap_or(Value::Null);
    let params = val.get("params").cloned().unwrap_or(Value::Null);
    let stdin = stdin.clone();
    match codex_approval(method, &params, &turn.items) {
        Some(approval) => {
            let key = id.to_string();
            let gate = gate.clone();
            let sid = session_id.to_string();
            let handle = tasks.spawn(async move {
                let choice = gate.ask(&sid, approval.prompt).await;
                let reply = approval_response(&id, &approval.decisions, choice.as_deref());
                // Detach the write so a late `serverRequest/resolved` abort
                // cannot truncate a JSON line mid-frame.
                tokio::spawn(async move { write_json_line(&stdin, &reply).await })
                    .await
                    .unwrap_or(Ok(()))
            });
            Some((key, handle))
        }
        // Permission-profile requests expect a granted permissions object
        // rather than a `decision` string.
        None if method == "item/permissions/requestApproval" => {
            let key = id.to_string();
            let requested = params
                .get("permissions")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let mut desc = serde_json::to_string_pretty(&requested).unwrap_or_default();
            if let Some(reason) = params
                .get("reason")
                .and_then(Value::as_str)
                .filter(|r| !r.trim().is_empty())
            {
                desc = format!("{reason}\n\n{desc}");
            }
            let prompt = ApprovalPrompt {
                tool_call_id: params
                    .get("itemId")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                title: "Grant additional permissions".to_string(),
                description: desc,
                options: vec![
                    permissions::option("allow", "Allow for this turn", "allow_always"),
                    permissions::option("deny", "Deny", "reject_once"),
                ],
            };
            let gate = gate.clone();
            let sid = session_id.to_string();
            let handle = tasks.spawn(async move {
                let choice = gate.ask(&sid, prompt).await;
                let result = if choice.as_deref() == Some("allow") {
                    json!({"permissions": requested, "scope": "turn"})
                } else {
                    json!({"permissions": {}})
                };
                let reply = json!({"jsonrpc": "2.0", "id": id, "result": result});
                tokio::spawn(async move { write_json_line(&stdin, &reply).await })
                    .await
                    .unwrap_or(Ok(()))
            });
            Some((key, handle))
        }
        // MCP servers asking for input are declined; the tool call fails
        // instead of the turn waiting.
        None if method == "mcpServer/elicitation/request" => {
            let reply = json!({"jsonrpc": "2.0", "id": id, "result": {"action": "decline"}});
            tasks.spawn(async move { write_json_line(&stdin, &reply).await });
            None
        }
        None => {
            let reply = json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": format!("SAME does not support {method}")},
            });
            tasks.spawn(async move { write_json_line(&stdin, &reply).await });
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_approval_maps_available_decisions() {
        let params = json!({
            "itemId": "exec-1",
            "command": "/usr/bin/zsh -lc 'touch a.txt'",
            "cwd": "/w",
            "commandActions": [{"type": "unknown", "command": "touch a.txt"}],
            "availableDecisions": [
                "accept",
                {"acceptWithExecpolicyAmendment": {"execpolicy_amendment": ["touch", "a.txt"]}},
                "cancel"
            ],
        });
        let a = codex_approval(
            "item/commandExecution/requestApproval",
            &params,
            &HashMap::new(),
        )
        .unwrap();
        assert_eq!(a.prompt.title, "Run: touch a.txt");
        assert_eq!(a.prompt.tool_call_id.as_deref(), Some("exec-1"));
        let kinds: Vec<&str> = a.prompt.options.iter().map(|o| o.kind.as_str()).collect();
        assert_eq!(
            kinds,
            ["allow_once", "allow_always", "reject_once", "reject_always"]
        );
        assert_eq!(a.prompt.options[1].name, "Always allow `touch a.txt`");

        let id = json!(7);
        let pick =
            |c: Option<&str>| approval_response(&id, &a.decisions, c)["result"]["decision"].clone();
        assert_eq!(pick(Some("0")), json!("accept"));
        assert_eq!(
            pick(Some("1"))["acceptWithExecpolicyAmendment"]["execpolicy_amendment"],
            json!(["touch", "a.txt"])
        );
        assert_eq!(pick(Some("2")), json!("decline"));
        assert_eq!(pick(Some("3")), json!("cancel"));
        assert_eq!(pick(None), json!("cancel"));
    }

    #[test]
    fn file_change_approval_uses_cached_item() {
        let mut items = HashMap::new();
        items.insert(
            "p1".to_string(),
            json!({"type": "fileChange", "id": "p1", "changes": [
                {"path": "/w/a.txt", "kind": {"type": "add"}, "diff": "hi\n"}
            ], "status": "inProgress"}),
        );
        let a = codex_approval(
            "item/fileChange/requestApproval",
            &json!({"itemId": "p1", "reason": null}),
            &items,
        )
        .unwrap();
        assert_eq!(a.prompt.title, "Edit /w/a.txt");
        assert!(a.prompt.description.contains("hi"));
        let kinds: Vec<&str> = a.prompt.options.iter().map(|o| o.kind.as_str()).collect();
        assert_eq!(kinds, ["allow_once", "reject_once", "reject_always"]);
    }

    #[test]
    fn unknown_server_requests_are_not_approvals() {
        assert!(
            codex_approval("item/tool/requestUserInput", &json!({}), &HashMap::new()).is_none()
        );
    }

    #[test]
    fn notifications_stream_text_and_end_turn() {
        let events = EventBus::default();
        let mut rx = events.subscribe();
        let mut turn = CodexTurn::default();
        let n = |m: &str, p: Value| json!({"jsonrpc": "2.0", "method": m, "params": p});
        for (m, p) in [
            (
                "item/agentMessage/delta",
                json!({"itemId": "a", "delta": "Hel"}),
            ),
            (
                "item/agentMessage/delta",
                json!({"itemId": "a", "delta": "lo"}),
            ),
            (
                "item/completed",
                json!({"item": {"type": "agentMessage", "id": "a", "text": "Hello"}}),
            ),
            (
                "item/completed",
                json!({"item": {"type": "agentMessage", "id": "b", "text": "Done."}}),
            ),
            (
                "item/completed",
                json!({"item": {"type": "commandExecution", "id": "c", "command": "x", "status": "declined"}}),
            ),
        ] {
            assert!(handle_notification(m, &n(m, p), "s", &events, &mut turn).is_none());
        }
        let mut text = String::new();
        let mut denied = false;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                SameEvent::MessageDelta { text: t, .. } => text.push_str(&t),
                SameEvent::ToolCallUpdated { tool_call, .. } => {
                    denied = tool_call.status == ToolCallStatus::Denied
                }
                _ => {}
            }
        }
        assert_eq!(text, "Hello\n\nDone.");
        assert!(denied);

        // Another thread's (a sub-agent's) completion does not end this turn.
        turn.thread_id = Some("main".into());
        assert!(handle_notification(
            "turn/completed",
            &n(
                "turn/completed",
                json!({"threadId": "sub", "turn": {"status": "completed"}})
            ),
            "s",
            &events,
            &mut turn,
        )
        .is_none());

        let end = handle_notification(
            "turn/completed",
            &n(
                "turn/completed",
                json!({"turn": {"status": "failed", "error": {"message": "boom"}}}),
            ),
            "s",
            &events,
            &mut turn,
        );
        assert_eq!(end, Some(TurnEnd::Failed("boom".into())));
    }
}
