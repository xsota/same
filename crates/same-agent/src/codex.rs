//! Codex through `codex app-server` (JSON-RPC over stdio), one process per
//! SAME session kept between turns. Each turn carries the session's approval
//! mode (approval policy, sandbox, plan collaboration mode); command and
//! file-change approvals and `request_user_input` questions go to the user.

use crate::adapters::{write_json_line, ArgList, SharedStdin};
use crate::diff;
use crate::permissions::{self, ApprovalPrompt, AutoAllow, PermissionGate};
use crate::process::{LineProcess, Pool, Pooled};
use anyhow::{anyhow, bail, Result};
use same_acp::exec::ExecTarget;
use same_core::{
    ApprovalMode, EventBus, ProviderConfig, SameEvent, ToolCallRecord, ToolCallStatus,
    UserQuestion, UserQuestionOption,
};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::task::JoinSet;

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

/// The app-server of one SAME session.
pub(crate) struct CodexProcess {
    proc: LineProcess,
    fingerprint: String,
    next_id: u64,
    /// The thread this process has open, and its model (needed to name the
    /// model in a collaboration mode).
    thread_id: Option<String>,
    thread_model: Option<String>,
}

impl Pooled for CodexProcess {
    fn process(&mut self) -> &mut LineProcess {
        &mut self.proc
    }
    fn into_process(self) -> LineProcess {
        self.proc
    }
    fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}

pub(crate) type CodexPool = Pool<CodexProcess>;

impl CodexProcess {
    fn request(&mut self, method: &str, params: Value) -> (u64, Value) {
        self.next_id += 1;
        let id = self.next_id;
        (
            id,
            json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
        )
    }

    /// Sends a request and waits for its response outside a turn. Server
    /// requests arriving meanwhile are refused; notifications are dropped.
    async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let (id, msg) = self.request(method, params);
        write_json_line(&self.proc.stdin, &msg).await?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        loop {
            let line = tokio::time::timeout_at(deadline, self.proc.lines.recv())
                .await
                .map_err(|_| anyhow!("Codex app-server did not answer {method}"))?;
            let Some(line) = line else {
                let code = self.proc.wait_exit().await;
                let tail = self.proc.stderr_tail();
                let last = tail.iter().rev().find(|l| !l.trim().is_empty());
                bail!(
                    "Codex app-server exited with status {}{}",
                    code.unwrap_or(-1),
                    last.map(|l| format!(": {}", l.trim())).unwrap_or_default()
                );
            };
            let Ok(val) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            let method_in = val.get("method").and_then(Value::as_str);
            let has_id = val.get("id").is_some_and(|i| !i.is_null());
            match (method_in, has_id) {
                (Some(m), true) => {
                    let reply = unsupported_request(&val, m);
                    write_json_line(&self.proc.stdin, &reply).await?;
                }
                (None, true) if val.get("id").and_then(Value::as_u64) == Some(id) => {
                    if let Some(err) = val.get("error") {
                        bail!(
                            "{}",
                            err.get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("Codex request failed")
                        );
                    }
                    return Ok(val.get("result").cloned().unwrap_or(Value::Null));
                }
                _ => {}
            }
        }
    }
}

/// The settings one turn runs with, from the session's approval mode.
#[derive(Debug, PartialEq)]
pub(crate) struct ModeSettings {
    pub approval_policy: &'static str,
    pub sandbox_policy: Value,
    pub plan: bool,
}

pub(crate) fn codex_mode_settings(mode: ApprovalMode) -> ModeSettings {
    match mode {
        ApprovalMode::Plan => ModeSettings {
            approval_policy: "on-request",
            sandbox_policy: json!({"type": "readOnly"}),
            plan: true,
        },
        // Codex has no edits-only mode: inside the workspace it already
        // writes without asking, and a write outside it is not an "edit" to
        // wave through.
        ApprovalMode::Ask | ApprovalMode::AcceptEdits => ModeSettings {
            approval_policy: "on-request",
            sandbox_policy: json!({"type": "workspaceWrite"}),
            plan: false,
        },
        ApprovalMode::FullAccess => ModeSettings {
            approval_policy: "never",
            sandbox_policy: json!({"type": "dangerFullAccess"}),
            plan: false,
        },
    }
}

fn codex_fingerprint(provider: &ProviderConfig, cwd: &Path, target: &ExecTarget) -> String {
    format!(
        "{}\u{0}{:?}\u{0}{:?}\u{0}{}\u{0}{:?}",
        provider.command,
        provider.args,
        provider.env,
        cwd.display(),
        target,
    )
}

/// How long a stopped turn may take to wind down before the process is
/// killed instead.
const INTERRUPT_GRACE: Duration = Duration::from_secs(10);

pub(crate) struct CodexTurnRequest<'a> {
    pub pool: &'a CodexPool,
    pub provider: &'a ProviderConfig,
    pub session_id: &'a str,
    pub external_session_id: Option<&'a str>,
    pub model: Option<&'a str>,
    pub effort: Option<&'a str>,
    pub cwd: &'a Path,
    pub target: &'a ExecTarget,
    pub prompt: &'a str,
    pub images: &'a [String],
    pub gate: &'a PermissionGate,
    pub events: &'a EventBus,
}

/// Runs one turn on the session's app-server, starting one (and resuming
/// its thread) when there is none to reuse.
pub(crate) async fn run_codex_turn(
    t: CodexTurnRequest<'_>,
    mut cancel_rx: oneshot::Receiver<()>,
) -> Result<()> {
    let fingerprint = codex_fingerprint(t.provider, t.cwd, t.target);
    let mut lease = t.pool.lease(t.session_id).await;
    if lease.reusable(&fingerprint).await.is_none() {
        let mut args = ArgList::new(&t.provider.args);
        args.arg("app-server");
        let proc = LineProcess::spawn(t.provider, &args.0, t.cwd, t.target, t.events, t.session_id)
            .await?;
        let mut process = CodexProcess {
            proc,
            fingerprint,
            next_id: 0,
            thread_id: None,
            thread_model: None,
        };
        let init = tokio::select! {
            _ = &mut cancel_rx => {
                process.proc.shutdown().await;
                t.events.publish(SameEvent::SessionCancelled {
                    session_id: t.session_id.to_string(),
                });
                return Ok(());
            }
            res = process.call(
                "initialize",
                json!({
                    "clientInfo": {"name": "same", "version": env!("CARGO_PKG_VERSION")},
                    // Needed for `collaborationMode` (plan mode).
                    "capabilities": {"experimentalApi": true},
                }),
            ) => res,
        };
        if let Err(err) = init {
            process.proc.shutdown().await;
            return Err(err);
        }
        write_json_line(
            &process.proc.stdin,
            &json!({"jsonrpc": "2.0", "method": "initialized"}),
        )
        .await?;
        lease.put(process);
    }
    let process = lease.get().expect("process just ensured");

    let open = tokio::select! {
        _ = &mut cancel_rx => {
            lease.discard().await;
            t.events.publish(SameEvent::SessionCancelled {
                session_id: t.session_id.to_string(),
            });
            return Ok(());
        }
        res = open_thread(process, &t) => res,
    };
    if let Err(err) = open {
        lease.discard().await;
        return Err(err);
    }
    if cancel_rx.try_recv().is_ok() {
        lease.discard().await;
        t.events.publish(SameEvent::SessionCancelled {
            session_id: t.session_id.to_string(),
        });
        return Ok(());
    }

    let res = run_turn_on(process, &t, &mut cancel_rx).await;
    match res {
        Ok((end, process_ok)) => {
            if !process_ok {
                lease.discard().await;
            }
            match end {
                TurnEnd::Completed => {
                    t.events.publish(SameEvent::SessionCompleted {
                        session_id: t.session_id.to_string(),
                        stop_reason: Some("end_turn".to_string()),
                    });
                    Ok(())
                }
                // Stopped by the user, or "Deny and stop".
                TurnEnd::Interrupted => {
                    t.events.publish(SameEvent::SessionCancelled {
                        session_id: t.session_id.to_string(),
                    });
                    Ok(())
                }
                TurnEnd::Failed(err) => Err(anyhow!(err)),
            }
        }
        Err(err) => {
            lease.discard().await;
            Err(err)
        }
    }
}

/// Makes sure the process has the session's thread open: resumes the stored
/// one, or starts a new one when there is none or it is gone.
async fn open_thread(process: &mut CodexProcess, t: &CodexTurnRequest<'_>) -> Result<()> {
    let wanted = t.external_session_id.filter(|s| !s.trim().is_empty());
    if process.thread_id.is_some() && (wanted.is_none() || wanted == process.thread_id.as_deref()) {
        return Ok(());
    }
    let cwd = t.target.to_target_path(t.cwd);
    let base = json!({
        "cwd": cwd,
        "approvalPolicy": "on-request",
        "sandbox": "workspace-write",
    });
    let result = match wanted {
        Some(thread_id) => {
            let mut params = base.clone();
            params["threadId"] = json!(thread_id);
            match process.call("thread/resume", params).await {
                Ok(r) => r,
                Err(err) if err.to_string().contains("no rollout found") => {
                    t.events.publish(SameEvent::AgentLog {
                        session_id: Some(t.session_id.to_string()),
                        provider_id: t.provider.id.clone(),
                        line: format!("Codex thread {thread_id} not found; starting a new thread"),
                    });
                    process.call("thread/start", base).await?
                }
                Err(err) => return Err(err),
            }
        }
        None => process.call("thread/start", base).await?,
    };
    let thread_id = result
        .pointer("/thread/id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Codex did not return a thread id"))?
        .to_string();
    process.thread_model = result
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string);
    t.events.publish(SameEvent::ExternalSessionBound {
        session_id: t.session_id.to_string(),
        external_session_id: thread_id.clone(),
    });
    process.thread_id = Some(thread_id);
    Ok(())
}

fn turn_start_params(
    thread_id: &str,
    t: &CodexTurnRequest<'_>,
    thread_model: Option<&str>,
    mode: ApprovalMode,
) -> Value {
    let mut input = vec![json!({"type": "text", "text": t.prompt})];
    input.extend(
        t.images
            .iter()
            .map(|path| json!({"type": "localImage", "path": path})),
    );
    let settings = codex_mode_settings(mode);
    let model = t.model.filter(|s| !s.trim().is_empty());
    let effort = t.effort.filter(|s| !s.trim().is_empty());
    let mut params = json!({
        "threadId": thread_id,
        "input": input,
        "approvalPolicy": settings.approval_policy,
        "sandboxPolicy": settings.sandbox_policy,
    });
    if let Some(m) = model {
        params["model"] = json!(m);
    }
    if let Some(e) = effort {
        params["effort"] = json!(e);
    }
    // Sent every turn (plan or default) so leaving plan mode clears it. It
    // takes precedence over model and effort, so it repeats them.
    if let Some(m) = model.or(thread_model) {
        let mut mode_settings = json!({"model": m});
        if let Some(e) = effort {
            mode_settings["reasoning_effort"] = json!(e);
        }
        params["collaborationMode"] = json!({
            "mode": if settings.plan { "plan" } else { "default" },
            "settings": mode_settings,
        });
    }
    params
}

/// Runs the turn; returns how it ended and whether the process can take
/// another turn.
async fn run_turn_on(
    process: &mut CodexProcess,
    t: &CodexTurnRequest<'_>,
    cancel_rx: &mut oneshot::Receiver<()>,
) -> Result<(TurnEnd, bool)> {
    let session_id = t.session_id;
    let events = t.events;
    process.proc.drain();
    let stdin = process.proc.stdin.clone();
    let thread_id = process.thread_id.clone().expect("thread opened");
    let mode = t.gate.mode(session_id);
    let params = turn_start_params(&thread_id, t, process.thread_model.as_deref(), mode);
    let (turn_req_id, msg) = process.request("turn/start", params);
    write_json_line(&stdin, &msg).await?;

    let mut turn = CodexTurn {
        thread_id: Some(thread_id.clone()),
        ..CodexTurn::default()
    };
    let mut request_tasks: JoinSet<Result<()>> = JoinSet::new();
    let mut pending_requests: HashMap<String, tokio::task::AbortHandle> = HashMap::new();
    let mut stopping: Option<tokio::time::Instant> = None;
    let mut interrupt_sent = false;

    loop {
        // Interrupt once the turn id is known (it may not be yet when the
        // user stops right after sending).
        if stopping.is_some() && !interrupt_sent {
            if let Some(turn_id) = turn.turn_id.clone() {
                let (_, msg) = process.request(
                    "turn/interrupt",
                    json!({"threadId": thread_id, "turnId": turn_id}),
                );
                write_json_line(&stdin, &msg).await?;
                interrupt_sent = true;
            }
        }
        let deadline = stopping;
        tokio::select! {
            Some(res) = request_tasks.join_next(), if !request_tasks.is_empty() => {
                if let Ok(Err(err)) = res {
                    return Err(anyhow!("Codex stdin write failed: {err}"));
                }
            }
            _ = &mut *cancel_rx, if stopping.is_none() => {
                request_tasks.abort_all();
                pending_requests.clear();
                stopping = Some(tokio::time::Instant::now() + INTERRUPT_GRACE);
            }
            _ = sleep_until_opt(deadline) => {
                return Ok((TurnEnd::Interrupted, false));
            }
            line = process.proc.lines.recv() => {
                let Some(line) = line else {
                    if stopping.is_some() {
                        return Ok((TurnEnd::Interrupted, false));
                    }
                    let code = process.proc.wait_exit().await;
                    let tail = process.proc.stderr_tail();
                    let mut msg = format!(
                        "Codex app-server exited with status {} before the turn finished",
                        code.unwrap_or(-1)
                    );
                    if let Some(err) = turn.error.take() {
                        msg.push_str(&format!(": {err}"));
                    } else if let Some(last) = tail.iter().rev().find(|l| !l.trim().is_empty()) {
                        msg.push_str(&format!(": {}", last.trim()));
                    }
                    return Ok((TurnEnd::Failed(msg), false));
                };
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let Ok(val) = serde_json::from_str::<Value>(trimmed) else {
                    events.publish(SameEvent::AgentLog {
                        session_id: Some(session_id.to_string()),
                        provider_id: t.provider.id.clone(),
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
                        if stopping.is_some() {
                            let reply = dismissed_reply(&val, method);
                            write_json_line(&stdin, &reply).await?;
                        } else if let Some((key, handle)) = spawn_server_request(
                            method,
                            &val,
                            &turn,
                            session_id,
                            t.gate,
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
                            handle_notification(method, &val, session_id, events, &mut turn)
                        {
                            request_tasks.abort_all();
                            let end = if stopping.is_some() { TurnEnd::Interrupted } else { end };
                            return Ok((end, true));
                        }
                    }
                    (None, true) => {
                        if val.get("id").and_then(Value::as_u64) == Some(turn_req_id) {
                            if let Some(err) = val.get("error") {
                                let msg = err
                                    .get("message")
                                    .and_then(Value::as_str)
                                    .unwrap_or("Codex could not start the turn");
                                return Ok((TurnEnd::Failed(msg.to_string()), true));
                            }
                            if let Some(id) =
                                val.pointer("/result/turn/id").and_then(Value::as_str)
                            {
                                turn.turn_id = Some(id.to_string());
                            }
                        }
                    }
                    (None, false) => {}
                }
            }
        }
    }
}

async fn sleep_until_opt(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending().await,
    }
}

/// The answer to a server request that arrives while the turn is stopping.
fn dismissed_reply(val: &Value, method: &str) -> Value {
    let id = val.get("id").cloned().unwrap_or(Value::Null);
    match method {
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
            json!({"jsonrpc": "2.0", "id": id, "result": {"decision": "cancel"}})
        }
        "item/tool/requestUserInput" => {
            json!({"jsonrpc": "2.0", "id": id, "result": {"answers": {}}})
        }
        _ => unsupported_request(val, method),
    }
}

fn unsupported_request(val: &Value, method: &str) -> Value {
    let id = val.get("id").cloned().unwrap_or(Value::Null);
    if method == "mcpServer/elicitation/request" {
        return json!({"jsonrpc": "2.0", "id": id, "result": {"action": "decline"}});
    }
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": -32601, "message": format!("SAME does not support {method}")},
    })
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
                "plan" if completed => {
                    let plan = item
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim();
                    if !plan.is_empty() {
                        events.publish(SameEvent::PlanProposed {
                            session_id: session_id.to_string(),
                            plan: plan.to_string(),
                        });
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

/// The changes of a `fileChange` item as one unified diff. Codex gives an
/// added file's content and a deleted file's old content rather than a diff.
fn unified_changes(item: &Value) -> Option<String> {
    let changes = item.get("changes")?.as_array()?;
    let out: String = changes
        .iter()
        .map(|c| {
            let path = c.get("path").and_then(Value::as_str).unwrap_or("file");
            let body = c.get("diff").and_then(Value::as_str).unwrap_or("");
            match c.pointer("/kind/type").and_then(Value::as_str) {
                Some("add") => diff::unified(path, "", body),
                Some("delete") => diff::unified(path, body, ""),
                _ if body.starts_with("---") => body.to_string(),
                _ => {
                    let mut d = format!("--- a/{path}\n+++ b/{path}\n{body}");
                    if !d.ends_with('\n') {
                        d.push('\n');
                    }
                    d
                }
            }
        })
        .collect();
    (!out.trim().is_empty()).then_some(out)
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
    let mut offered_session = false;
    for d in &available {
        match d.as_str() {
            Some("accept") => {
                push(d.clone(), "Allow".into(), "allow_once");
                // The app-server outlives the turn, so a session grant is
                // worth offering; Codex honours it even when unlisted.
                push(
                    json!("acceptForSession"),
                    "Allow for this session".into(),
                    "allow_always",
                );
                offered_session = true;
            }
            Some("acceptForSession") if !offered_session => {
                push(d.clone(), "Allow for this session".into(), "allow_always");
                offered_session = true;
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

    let diff = match method {
        "item/fileChange/requestApproval" => item.and_then(unified_changes),
        _ => None,
    };
    Some(CodexApproval {
        prompt: ApprovalPrompt {
            tool_call_id: item_id.map(str::to_string),
            title,
            description,
            options,
            diff,
            // Codex's approval policy follows the mode each turn; this covers
            // a switch to full access mid-turn.
            auto: AutoAllow::InFullAccess,
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
                diff: None,
                auto: AutoAllow::Never,
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
        None if method == "item/tool/requestUserInput" => {
            let key = id.to_string();
            let questions = codex_questions(&params);
            let gate = gate.clone();
            let sid = session_id.to_string();
            let handle = tasks.spawn(async move {
                let answers = gate.ask_user(&sid, questions.clone()).await;
                let reply = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {"answers": codex_answers(&questions, answers)},
                });
                tokio::spawn(async move { write_json_line(&stdin, &reply).await })
                    .await
                    .unwrap_or(Ok(()))
            });
            Some((key, handle))
        }
        // Anything else is refused (MCP elicitations declined) so the turn
        // does not wait forever.
        None => {
            let reply = unsupported_request(val, method);
            tasks.spawn(async move { write_json_line(&stdin, &reply).await });
            None
        }
    }
}

/// The questions of a `request_user_input` request.
fn codex_questions(params: &Value) -> Vec<UserQuestion> {
    params
        .get("questions")
        .and_then(Value::as_array)
        .map(|qs| {
            qs.iter()
                .filter_map(|q| {
                    let id = q.get("id").and_then(Value::as_str)?.to_string();
                    let options: Vec<UserQuestionOption> = q
                        .get("options")
                        .and_then(Value::as_array)
                        .map(|opts| {
                            opts.iter()
                                .filter_map(|o| {
                                    Some(UserQuestionOption {
                                        label: o.get("label").and_then(Value::as_str)?.to_string(),
                                        description: o
                                            .get("description")
                                            .and_then(Value::as_str)
                                            .unwrap_or("")
                                            .to_string(),
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    Some(UserQuestion {
                        header: q
                            .get("header")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        question: q
                            .get("question")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        multi_select: false,
                        // Without options, the answer can only be typed.
                        allow_other: options.is_empty()
                            || q.get("isOther").and_then(Value::as_bool) == Some(true),
                        secret: q.get("isSecret").and_then(Value::as_bool) == Some(true),
                        options,
                        id,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `{question id: {answers: [...]}}`; empty when the card was dismissed.
fn codex_answers(questions: &[UserQuestion], answers: Option<Vec<Vec<String>>>) -> Value {
    let map: serde_json::Map<String, Value> = answers
        .map(|answers| {
            questions
                .iter()
                .zip(answers)
                .map(|(q, a)| (q.id.clone(), json!({"answers": a})))
                .collect()
        })
        .unwrap_or_default();
    Value::Object(map)
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
            [
                "allow_once",
                "allow_always",
                "allow_always",
                "reject_once",
                "reject_always"
            ]
        );
        assert_eq!(a.prompt.options[1].name, "Allow for this session");
        assert_eq!(a.prompt.options[2].name, "Always allow `touch a.txt`");
        assert_eq!(a.prompt.auto, AutoAllow::InFullAccess);

        let id = json!(7);
        let pick =
            |c: Option<&str>| approval_response(&id, &a.decisions, c)["result"]["decision"].clone();
        assert_eq!(pick(Some("0")), json!("accept"));
        assert_eq!(pick(Some("1")), json!("acceptForSession"));
        assert_eq!(
            pick(Some("2"))["acceptWithExecpolicyAmendment"]["execpolicy_amendment"],
            json!(["touch", "a.txt"])
        );
        assert_eq!(pick(Some("3")), json!("decline"));
        assert_eq!(pick(Some("4")), json!("cancel"));
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
        let diff = a.prompt.diff.unwrap();
        assert!(diff.contains("+++ b//w/a.txt") && diff.contains("+hi"));
        let kinds: Vec<&str> = a.prompt.options.iter().map(|o| o.kind.as_str()).collect();
        assert_eq!(
            kinds,
            ["allow_once", "allow_always", "reject_once", "reject_always"]
        );
    }

    #[test]
    fn update_diffs_get_headers() {
        let item = json!({"changes": [
            {"path": "a.rs", "kind": {"type": "update", "move_path": null}, "diff": "@@ -1 +1 @@\n-x\n+y\n"},
            {"path": "b.rs", "kind": {"type": "delete"}, "diff": "gone\n"}
        ]});
        let d = unified_changes(&item).unwrap();
        assert!(d.contains("--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@"));
        assert!(d.contains("-gone"));
    }

    #[test]
    fn mode_settings_per_turn() {
        let plan = codex_mode_settings(ApprovalMode::Plan);
        assert!(plan.plan);
        assert_eq!(plan.sandbox_policy["type"], "readOnly");
        assert_eq!(
            codex_mode_settings(ApprovalMode::Ask),
            codex_mode_settings(ApprovalMode::AcceptEdits)
        );
        assert_eq!(
            codex_mode_settings(ApprovalMode::Ask).approval_policy,
            "on-request"
        );
        let full = codex_mode_settings(ApprovalMode::FullAccess);
        assert_eq!(full.approval_policy, "never");
        assert_eq!(full.sandbox_policy["type"], "dangerFullAccess");
    }

    #[test]
    fn questions_map_both_ways() {
        let params = json!({"questions": [
            {"id": "len", "header": "README", "question": "Short or long?", "isOther": true,
             "isSecret": false, "options": [{"label": "Short", "description": "d"}, {"label": "Long", "description": "e"}]},
            {"id": "key", "header": "Key", "question": "API key?", "isSecret": true, "options": null}
        ]});
        let qs = codex_questions(&params);
        assert_eq!(qs.len(), 2);
        assert!(qs[0].allow_other && !qs[0].secret && qs[0].options.len() == 2);
        assert!(qs[1].allow_other && qs[1].secret);
        let a = codex_answers(&qs, Some(vec![vec!["Short".into()], vec!["k-1".into()]]));
        assert_eq!(
            a,
            json!({"len": {"answers": ["Short"]}, "key": {"answers": ["k-1"]}})
        );
        assert_eq!(codex_answers(&qs, None), json!({}));
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
