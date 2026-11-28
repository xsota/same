//! Claude Code through `claude -p` with `stream-json` on stdin and stdout,
//! one process per SAME session kept between turns. Permission prompts,
//! questions (AskUserQuestion) and plans (ExitPlanMode) arrive as
//! `can_use_tool` control requests (`--permission-prompt-tool stdio`).

use crate::adapters::{summarize_tool_call, write_json_line, ArgList, SharedStdin};
use crate::diff;
use crate::permissions::{self, ApprovalPrompt, AutoAllow, PermissionGate};
use crate::process::{LineProcess, Pool, Pooled};
use anyhow::{anyhow, Result};
use same_acp::exec::ExecTarget;
use same_core::{
    ApprovalMode, EventBus, ProviderConfig, SameEvent, ToolCallRecord, ToolCallStatus,
    UserQuestion, UserQuestionOption,
};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::task::JoinSet;

/// How one turn ended.
enum ClaudeRun {
    Completed,
    /// `process_ok` is false when the process had to be killed.
    Cancelled {
        process_ok: bool,
    },
    /// Mode switch could not be confirmed on the pooled process; restart it
    /// with the requested `--permission-mode` before running the turn.
    RestartForMode,
    Failed {
        code: Option<i32>,
        /// `errors` of an `is_error` result line.
        errors: Vec<String>,
        stderr_tail: Vec<String>,
        /// True if the turn already emitted assistant output or tool activity.
        did_work: bool,
        /// The process exited (it cannot take another turn).
        exited: bool,
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
    let mut msg = match code {
        Some(code) => format!("Claude CLI exited with status {code}"),
        None => "Claude Code failed".to_string(),
    };
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

/// The Claude Code process of one SAME session.
pub(crate) struct ClaudeProcess {
    proc: LineProcess,
    fingerprint: String,
    /// The CLI's confirmed `--permission-mode`, shared with [`ClaudeControls`]
    /// so a mode change can reach it mid-turn.
    mode: Arc<std::sync::Mutex<String>>,
    /// In-flight `set_permission_mode` control requests (request_id -> mode).
    pending_modes: Arc<std::sync::Mutex<HashMap<String, String>>>,
    /// Set when a mode switch fails or is rejected so the next turn discards
    /// this process instead of reusing it with stale permissions.
    mode_dirty: Arc<AtomicBool>,
}

impl Pooled for ClaudeProcess {
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

pub(crate) type ClaudePool = Pool<ClaudeProcess>;

/// Per session: the running CLI's stdin and permission mode, to switch the
/// mode while a turn runs.
#[derive(Clone, Default)]
pub(crate) struct ClaudeControls(Arc<std::sync::Mutex<HashMap<String, ClaudeControl>>>);

#[derive(Clone)]
struct ClaudeControl {
    stdin: SharedStdin,
    mode: Arc<std::sync::Mutex<String>>,
    pending_modes: Arc<std::sync::Mutex<HashMap<String, String>>>,
    mode_dirty: Arc<AtomicBool>,
}

impl ClaudeControls {
    /// Sends `set_permission_mode` to the session's process, if it runs one.
    pub async fn set_mode(&self, session_id: &str, mode: ApprovalMode) {
        let entry = self.0.lock().unwrap().get(session_id).cloned();
        if let Some(ctrl) = entry {
            let wanted = claude_permission_mode(mode);
            let same_mode = *ctrl.mode.lock().unwrap() == wanted;
            let no_pending = ctrl.pending_modes.lock().unwrap().is_empty();
            if same_mode && no_pending {
                return;
            }
            let (rid, req) = set_mode_request(wanted);
            ctrl.pending_modes
                .lock()
                .unwrap()
                .insert(rid.clone(), wanted.to_string());
            if write_json_line(&ctrl.stdin, &req).await.is_err() {
                ctrl.pending_modes.lock().unwrap().remove(&rid);
                ctrl.mode_dirty.store(true, Ordering::SeqCst);
            }
        }
    }

    pub fn remove(&self, session_id: &str) {
        self.0.lock().unwrap().remove(session_id);
    }
}

/// Claude Code's `--permission-mode` for an approval mode.
pub fn claude_permission_mode(mode: ApprovalMode) -> &'static str {
    match mode {
        ApprovalMode::Plan => "plan",
        ApprovalMode::Ask => "default",
        ApprovalMode::AcceptEdits => "acceptEdits",
        ApprovalMode::FullAccess => "bypassPermissions",
    }
}

static NEXT_CONTROL_ID: AtomicU64 = AtomicU64::new(1);

fn control_request_with_id(request: Value) -> (String, Value) {
    let rid = format!("same-{}", NEXT_CONTROL_ID.fetch_add(1, Ordering::Relaxed));
    let val = json!({
        "type": "control_request",
        "request_id": rid,
        "request": request,
    });
    (rid, val)
}

fn control_request(request: Value) -> Value {
    control_request_with_id(request).1
}

fn set_mode_request(mode: &str) -> (String, Value) {
    control_request_with_id(json!({"subtype": "set_permission_mode", "mode": mode}))
}

/// What a process must have been started with to be reused for a turn:
/// model and effort are launch flags.
fn claude_fingerprint(
    provider: &ProviderConfig,
    model: Option<&str>,
    effort: Option<&str>,
    cwd: &Path,
    target: &ExecTarget,
    skill_args: &[String],
    skill_hash: &str,
) -> String {
    format!(
        "{}\u{0}{:?}\u{0}{:?}\u{0}{}\u{0}{:?}\u{0}{:?}\u{0}{:?}\u{0}{:?}\u{0}{}",
        provider.command,
        provider.args,
        provider.env,
        cwd.display(),
        target,
        model.unwrap_or(""),
        effort.unwrap_or(""),
        skill_args,
        skill_hash,
    )
}

/// How long a stopped turn may take to wind down before the process is
/// killed instead.
const INTERRUPT_GRACE: Duration = Duration::from_secs(10);

pub(crate) struct ClaudeTurn<'a> {
    pub pool: &'a ClaudePool,
    pub controls: &'a ClaudeControls,
    pub provider: &'a ProviderConfig,
    pub session_id: &'a str,
    pub external_session_id: Option<&'a str>,
    pub model: Option<&'a str>,
    pub effort: Option<&'a str>,
    pub cwd: &'a Path,
    pub target: &'a ExecTarget,
    pub prompt: &'a str,
    pub fallback_handoff_prompt: Option<&'a str>,
    pub skill_args: &'a [String],
    pub skill_hash: &'a str,
    pub gate: &'a PermissionGate,
    pub events: &'a EventBus,
}

/// Runs one turn on the session's Claude Code process, starting (or
/// resuming) one when there is none it can reuse.
pub(crate) async fn run_claude_turn(
    mut t: ClaudeTurn<'_>,
    mut cancel_rx: oneshot::Receiver<()>,
) -> Result<()> {
    let fingerprint = claude_fingerprint(
        t.provider,
        t.model,
        t.effort,
        t.cwd,
        t.target,
        t.skill_args,
        t.skill_hash,
    );
    let mut lease = t.pool.lease(t.session_id).await;
    let mut resume = t.external_session_id.filter(|s| !s.trim().is_empty());
    let mut mode_restarted = false;
    loop {
        let mut fresh = lease.reusable(&fingerprint).await.is_none();
        if !fresh
            && lease
                .get()
                .is_some_and(|p| p.mode_dirty.load(Ordering::SeqCst))
        {
            lease.discard().await;
            fresh = true;
        }
        if fresh {
            let process = spawn_claude(&t, resume, &fingerprint).await?;
            lease.put(process);
        }
        let process = lease.get().expect("process just ensured");
        let run = run_turn_on(process, &t, &mut cancel_rx).await;
        let run = match run {
            Ok(run) => run,
            Err(err) => {
                lease.discard().await;
                return Err(err);
            }
        };
        match run {
            ClaudeRun::Completed => {
                t.events.publish(SameEvent::SessionCompleted {
                    session_id: t.session_id.to_string(),
                    stop_reason: Some("end_turn".to_string()),
                });
                return Ok(());
            }
            ClaudeRun::RestartForMode => {
                lease.discard().await;
                if mode_restarted {
                    return Err(anyhow!("Claude CLI rejected permission mode change"));
                }
                mode_restarted = true;
            }
            ClaudeRun::Cancelled { process_ok } => {
                if !process_ok {
                    lease.discard().await;
                }
                t.events.publish(SameEvent::SessionCancelled {
                    session_id: t.session_id.to_string(),
                });
                return Ok(());
            }
            ref failed if fresh && resume.is_some_and(|id| failed.is_missing_conversation(id)) => {
                // The stored conversation is gone: start a fresh one with
                // context handoff if provided. Its new id replaces the stale
                // one through ExternalSessionBound / ProviderSessionBound.
                lease.discard().await;
                let missing_id = resume.unwrap_or_default().to_string();
                let reason =
                    format!("Conversation {missing_id} not found; starting a new Claude session");
                t.events.publish(SameEvent::AgentLog {
                    session_id: Some(t.session_id.to_string()),
                    provider_id: t.provider.id.clone(),
                    line: reason.clone(),
                });
                t.events.publish(SameEvent::NativeSessionFallback {
                    session_id: t.session_id.to_string(),
                    provider_id: t.provider.id.clone(),
                    reason,
                });
                if let Some(fb) = t.fallback_handoff_prompt {
                    t.prompt = fb;
                }
                resume = None;
            }
            ClaudeRun::Failed {
                code,
                errors,
                stderr_tail,
                exited,
                ..
            } => {
                if exited {
                    lease.discard().await;
                }
                return Err(anyhow!(claude_failure_message(code, &errors, &stderr_tail)));
            }
        }
    }
}

async fn spawn_claude(
    t: &ClaudeTurn<'_>,
    resume: Option<&str>,
    fingerprint: &str,
) -> Result<ClaudeProcess> {
    let mode = claude_permission_mode(t.gate.mode(t.session_id));
    let mut args = ArgList::new(&t.provider.args);
    args.arg("-p")
        .arg("--input-format")
        .arg("stream-json")
        .arg("--output-format")
        .arg("stream-json")
        .arg("--verbose")
        .arg("--permission-prompt-tool")
        .arg("stdio")
        // Lets the session switch to full access later without a restart;
        // the mode below decides whether it is on.
        .arg("--allow-dangerously-skip-permissions")
        .arg("--permission-mode")
        .arg(mode);
    if let Some(m) = t.model.filter(|s| !s.trim().is_empty()) {
        args.arg("--model").arg(m);
    }
    if let Some(e) = t.effort.filter(|s| !s.trim().is_empty()) {
        args.arg("--effort").arg(e);
    }
    if let Some(ext) = resume {
        args.arg("--resume").arg(ext);
    }
    for a in t.skill_args {
        args.arg(a);
    }
    let proc =
        LineProcess::spawn(t.provider, &args.0, t.cwd, t.target, t.events, t.session_id).await?;
    let mode = Arc::new(std::sync::Mutex::new(mode.to_string()));
    let pending_modes = Arc::new(std::sync::Mutex::new(HashMap::new()));
    let mode_dirty = Arc::new(AtomicBool::new(false));
    t.controls.0.lock().unwrap().insert(
        t.session_id.to_string(),
        ClaudeControl {
            stdin: proc.stdin.clone(),
            mode: mode.clone(),
            pending_modes: pending_modes.clone(),
            mode_dirty: mode_dirty.clone(),
        },
    );
    Ok(ClaudeProcess {
        proc,
        fingerprint: fingerprint.to_string(),
        mode,
        pending_modes,
        mode_dirty,
    })
}

/// One turn: sends the prompt and reads until its `result` line. Permission
/// prompts arrive as `can_use_tool` control requests (through
/// `--permission-prompt-tool stdio`) and are answered on stdin.
async fn run_turn_on(
    process: &mut ClaudeProcess,
    t: &ClaudeTurn<'_>,
    cancel_rx: &mut oneshot::Receiver<()>,
) -> Result<ClaudeRun> {
    let session_id = t.session_id;
    let events = t.events;
    // Drain any idle lines while still processing queued mode acknowledgments.
    while let Ok(line) = process.proc.lines.try_recv() {
        if let Ok(val) = serde_json::from_str::<Value>(line.trim()) {
            if val.get("type").and_then(Value::as_str) == Some("control_response") {
                handle_control_response(process, &val, None);
            }
        }
    }
    let stdin = process.proc.stdin.clone();

    let wanted = claude_permission_mode(t.gate.mode(session_id));
    let needs_mode_sync = *process.mode.lock().unwrap() != wanted
        || !process.pending_modes.lock().unwrap().is_empty();
    if needs_mode_sync {
        let (rid, req) = set_mode_request(wanted);
        process
            .pending_modes
            .lock()
            .unwrap()
            .insert(rid.clone(), wanted.to_string());
        if write_json_line(&stdin, &req).await.is_err() {
            process.pending_modes.lock().unwrap().remove(&rid);
            process.mode_dirty.store(true, Ordering::SeqCst);
            return Ok(ClaudeRun::RestartForMode);
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            tokio::select! {
                _ = &mut *cancel_rx => {
                    return Ok(ClaudeRun::Cancelled { process_ok: false });
                }
                _ = tokio::time::sleep_until(deadline) => {
                    process.mode_dirty.store(true, Ordering::SeqCst);
                    return Ok(ClaudeRun::RestartForMode);
                }
                line = process.proc.lines.recv() => {
                    let Some(line) = line else {
                        return Ok(ClaudeRun::RestartForMode);
                    };
                    let Ok(val) = serde_json::from_str::<Value>(line.trim()) else {
                        continue;
                    };
                    if val.get("type").and_then(Value::as_str) == Some("control_response") {
                        if let Some(ok) = handle_control_response(process, &val, Some(&rid)) {
                            if ok {
                                break;
                            } else {
                                return Ok(ClaudeRun::RestartForMode);
                            }
                        }
                    }
                }
            }
        }
    }
    // Written in the background: a large prompt must not block reading the
    // child's output.
    let mut prompt_task = {
        let stdin = stdin.clone();
        let message = claude_user_message(t.prompt);
        tokio::spawn(async move { write_json_line(&stdin, &message).await })
    };

    let mut state = ClaudeStreamState::default();
    // Open prompts, by control request id.
    let mut prompts: HashMap<String, tokio::task::AbortHandle> = HashMap::new();
    let mut prompt_tasks: JoinSet<(Option<String>, Result<()>)> = JoinSet::new();
    let mut prompt_done = false;
    let mut interrupt_deadline: Option<tokio::time::Instant> = None;

    loop {
        let deadline = interrupt_deadline;
        tokio::select! {
            Some(res) = prompt_tasks.join_next(), if !prompt_tasks.is_empty() => {
                if let Ok((rid, write_res)) = res {
                    if let Some(rid) = rid {
                        prompts.remove(&rid);
                    }
                    write_res?;
                }
            }
            write_res = &mut prompt_task, if !prompt_done => {
                prompt_done = true;
                if let Ok(Err(err)) = write_res {
                    return Err(err);
                }
            }
            _ = &mut *cancel_rx, if interrupt_deadline.is_none() => {
                prompt_task.abort();
                prompt_done = true;
                prompt_tasks.abort_all();
                prompts.clear();
                interrupt_deadline = Some(tokio::time::Instant::now() + INTERRUPT_GRACE);
                let sent = tokio::time::timeout(
                    INTERRUPT_GRACE,
                    write_json_line(&stdin, &control_request(json!({"subtype": "interrupt"}))),
                )
                .await;
                if !matches!(sent, Ok(Ok(()))) {
                    return Ok(ClaudeRun::Cancelled { process_ok: false });
                }
            }
            _ = sleep_until_opt(deadline) => {
                // The interrupt was not acknowledged in time.
                return Ok(ClaudeRun::Cancelled { process_ok: false });
            }
            line = process.proc.lines.recv() => {
                let Some(line) = line else {
                    // The process exited.
                    prompt_tasks.abort_all();
                    if interrupt_deadline.is_some() {
                        return Ok(ClaudeRun::Cancelled { process_ok: false });
                    }
                    let code = process.proc.wait_exit().await;
                    return Ok(ClaudeRun::Failed {
                        code,
                        errors: state.errors,
                        stderr_tail: process.proc.stderr_tail(),
                        did_work: state.did_work,
                        exited: true,
                    });
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
                match val.get("type").and_then(Value::as_str).unwrap_or("") {
                    "control_request" => {
                        if interrupt_deadline.is_some() {
                            // Stopping: refuse anything new.
                            if let Some(rid) = val.get("request_id").and_then(Value::as_str) {
                                let reply = claude_permission_response(
                                    rid,
                                    val.get("request").unwrap_or(&Value::Null),
                                    None,
                                );
                                write_json_line(&stdin, &reply).await?;
                            }
                        } else if let Some((rid, handle)) = spawn_claude_control_request(
                            &val,
                            session_id,
                            t.gate,
                            &stdin,
                            &state.denied,
                            events,
                            &mut prompt_tasks,
                        ) {
                            prompts.insert(rid, handle);
                        }
                    }
                    "control_cancel_request" => {
                        // Claude no longer needs the answer.
                        if let Some(handle) = val
                            .get("request_id")
                            .and_then(Value::as_str)
                            .and_then(|rid| prompts.remove(rid))
                        {
                            handle.abort();
                        }
                    }
                    "control_response" => {
                        handle_control_response(process, &val, None);
                        if process.mode_dirty.load(Ordering::SeqCst) {
                            prompt_task.abort();
                            prompt_tasks.abort_all();
                            return Err(anyhow!("Claude CLI rejected permission mode change"));
                        }
                    }
                    "result" => {
                        prompt_tasks.abort_all();
                        if interrupt_deadline.is_some() {
                            return Ok(ClaudeRun::Cancelled { process_ok: true });
                        }
                        handle_claude_json(&val, session_id, events, &mut state);
                        if state.is_error || !state.errors.is_empty() {
                            return Ok(ClaudeRun::Failed {
                                code: None,
                                errors: state.errors,
                                stderr_tail: process.proc.stderr_tail(),
                                did_work: state.did_work,
                                exited: !process.proc.is_alive(),
                            });
                        }
                        return Ok(ClaudeRun::Completed);
                    }
                    _ => handle_claude_json(&val, session_id, events, &mut state),
                }
            }
        }
    }
}

/// Matches a `control_response` against pending `set_permission_mode` requests.
/// Returns `Some(true)` if it confirmed `watch_id`, `Some(false)` if it rejected
/// `watch_id`, or `None` if it was for a different request.
fn handle_control_response(
    process: &ClaudeProcess,
    val: &Value,
    watch_id: Option<&str>,
) -> Option<bool> {
    let resp = val.get("response")?;
    let rid = resp.get("request_id").and_then(Value::as_str)?;
    let ok = resp.get("subtype").and_then(Value::as_str) == Some("success");
    let pending = process.pending_modes.lock().unwrap().remove(rid);
    if let Some(mode) = pending {
        if ok {
            *process.mode.lock().unwrap() = mode;
        } else {
            process.mode_dirty.store(true, Ordering::SeqCst);
        }
    }
    if watch_id == Some(rid) {
        Some(ok)
    } else {
        None
    }
}

async fn sleep_until_opt(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending().await,
    }
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

/// What Claude sees after proposing a plan; the user approves it (or asks
/// for changes) in their next message.
const CLAUDE_PLAN_MESSAGE: &str = "The user has been shown your plan. Stop here and wait: they will approve it or ask for changes in their next message.";

type PromptTasks = JoinSet<(Option<String>, Result<()>)>;

fn spawn_reply(tasks: &mut PromptTasks, stdin: &SharedStdin, reply: Value) {
    let stdin = stdin.clone();
    tasks.spawn(async move {
        let res = tokio::spawn(async move { write_json_line(&stdin, &reply).await })
            .await
            .unwrap_or(Ok(()));
        (None, res)
    });
}

/// Answers a control request from Claude Code. `can_use_tool` asks the user
/// in a background task (returned so it can be aborted): questions go to a
/// question card, a proposed plan to a plan card, anything else to an
/// approval prompt. Other requests get an error response right away.
fn spawn_claude_control_request(
    val: &Value,
    session_id: &str,
    gate: &PermissionGate,
    stdin: &SharedStdin,
    denied: &DeniedTools,
    events: &EventBus,
    tasks: &mut PromptTasks,
) -> Option<(String, tokio::task::AbortHandle)> {
    let request_id = val.get("request_id").and_then(Value::as_str)?.to_string();
    let request = val.get("request").cloned().unwrap_or(Value::Null);
    let subtype = request
        .get("subtype")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if subtype != "can_use_tool" {
        let reply = claude_control_error(
            &request_id,
            &format!("Unsupported control request: {subtype}"),
        );
        spawn_reply(tasks, stdin, reply);
        return None;
    }

    if request.get("tool_name").and_then(Value::as_str) == Some("ExitPlanMode") {
        let plan = request
            .pointer("/input/plan")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if !plan.is_empty() {
            events.publish(SameEvent::PlanProposed {
                session_id: session_id.to_string(),
                plan,
            });
            let reply = claude_success(
                &request_id,
                json!({"behavior": "deny", "message": CLAUDE_PLAN_MESSAGE}),
            );
            spawn_reply(tasks, stdin, reply);
            return None;
        }
        // No inline plan text (e.g. written to a file): fall through to
        // a normal approval prompt so the user can still review/decide.
    }

    match request.get("tool_name").and_then(Value::as_str) {
        Some("AskUserQuestion") => {
            let gate = gate.clone();
            let sid = session_id.to_string();
            let rid = request_id.clone();
            let stdin = stdin.clone();
            let handle = tasks.spawn(async move {
                let input = request.get("input").cloned().unwrap_or(Value::Null);
                let questions = claude_questions(&input);
                let answers = gate.ask_user(&sid, questions.clone()).await;
                let reply =
                    claude_success(&rid, claude_answers_response(&input, &questions, answers));
                let res = tokio::spawn(async move { write_json_line(&stdin, &reply).await })
                    .await
                    .unwrap_or(Ok(()));
                (Some(rid), res)
            });
            Some((request_id, handle))
        }
        _ => {
            let gate = gate.clone();
            let sid = session_id.to_string();
            let denied = denied.clone();
            let rid = request_id.clone();
            let stdin = stdin.clone();
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
                // Detach the write so a late `control_cancel_request` abort
                // cannot truncate a JSON line mid-frame.
                let res = tokio::spawn(async move { write_json_line(&stdin, &reply).await })
                    .await
                    .unwrap_or(Ok(()));
                (Some(rid), res)
            });
            Some((request_id, handle))
        }
    }
}

/// The questions of an AskUserQuestion call. Claude keys answers by the
/// question text, so that is the id.
fn claude_questions(input: &Value) -> Vec<UserQuestion> {
    input
        .get("questions")
        .and_then(Value::as_array)
        .map(|qs| {
            qs.iter()
                .enumerate()
                .filter_map(|(i, q)| {
                    let question = q
                        .get("question")
                        .and_then(Value::as_str)?
                        .trim()
                        .to_string();
                    if question.is_empty() {
                        return None;
                    }
                    let header = q
                        .get("header")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|h| !h.is_empty())
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("Question {}", i + 1));
                    let options = q
                        .get("options")
                        .and_then(Value::as_array)
                        .map(|opts| {
                            opts.iter()
                                .filter_map(|o| {
                                    let label = o.get("label").and_then(Value::as_str)?;
                                    Some(UserQuestionOption {
                                        label: label.to_string(),
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
                        id: question.clone(),
                        header,
                        question,
                        options,
                        multi_select: q.get("multiSelect").and_then(Value::as_bool) == Some(true),
                        allow_other: true,
                        secret: false,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The `can_use_tool` answer to AskUserQuestion: the questions with the
/// user's answers (several choices joined with ", "), or a denial when the
/// card was dismissed.
fn claude_answers_response(
    input: &Value,
    questions: &[UserQuestion],
    answers: Option<Vec<Vec<String>>>,
) -> Value {
    let Some(answers) = answers else {
        return json!({
            "behavior": "deny",
            "message": "The user dismissed the questions without answering.",
        });
    };
    let map: serde_json::Map<String, Value> = questions
        .iter()
        .zip(answers)
        .filter(|(_, a)| !a.is_empty())
        .map(|(q, a)| (q.id.clone(), Value::String(a.join(", "))))
        .collect();
    json!({
        "behavior": "allow",
        "updatedInput": {
            "questions": input.get("questions").cloned().unwrap_or_else(|| json!([])),
            "answers": map,
        },
    })
}

/// The approval prompt for a `can_use_tool` request.
fn claude_permission_prompt(request: &Value) -> ApprovalPrompt {
    let tool_name = request
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("Tool");
    let tool = request
        .get("display_name")
        .and_then(Value::as_str)
        .unwrap_or(tool_name);
    let input = request.get("input");
    let diff = input.and_then(|i| diff::claude_tool_diff(tool_name, i));
    let mut description = String::new();
    if let Some(d) = request
        .get("description")
        .and_then(Value::as_str)
        .filter(|d| !d.trim().is_empty())
    {
        description.push_str(d.trim());
    }
    if let Some(reason) = request
        .get("decision_reason")
        .or_else(|| request.get("decisionReason"))
        .and_then(Value::as_str)
        .filter(|d| !d.trim().is_empty())
    {
        if !description.is_empty() {
            description.push_str("\n\n");
        }
        description.push_str(reason.trim());
    }
    // The diff shows an edit better than its raw input, but preserve
    // scope-changing flags like `replace_all`.
    if diff.is_none() {
        if let Some(i) = input {
            if !description.is_empty() {
                description.push_str("\n\n");
            }
            description.push_str(&serde_json::to_string_pretty(i).unwrap_or_default());
        }
    } else if input
        .and_then(|i| i.get("replace_all"))
        .and_then(Value::as_bool)
        == Some(true)
    {
        if !description.is_empty() {
            description.push_str("\n\n");
        }
        description.push_str("replace_all: true (replaces all occurrences in the file)");
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
            "allow_session",
            "Allow for this session",
            "allow_always",
        ));
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
        diff,
        // Claude Code applies the session's permission mode itself; what it
        // still asks in full access are its safety checks.
        auto: AutoAllow::Never,
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
        // The same rules, kept only while this Claude Code process runs.
        Some("allow_session") => json!({
            "behavior": "allow",
            "updatedInput": input,
            "updatedPermissions": claude_rule_suggestions(request)
                .into_iter()
                .map(|mut s| {
                    s["destination"] = json!("session");
                    s
                })
                .collect::<Vec<_>>(),
        }),
        Some(_) => json!({"behavior": "deny", "message": CLAUDE_DENY_MESSAGE}),
        None => json!({
            "behavior": "deny",
            "message": CLAUDE_DENY_MESSAGE,
            "interrupt": true,
        }),
    };
    claude_success(request_id, decision)
}

fn claude_success(request_id: &str, response: Value) -> Value {
    json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": response,
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

#[cfg(test)]
mod tests {
    use super::*;
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
        assert_eq!(claude_failure_message(None, &[], &[]), "Claude Code failed");
        assert_eq!(
            claude_failure_message(None, &["API error".into()], &[]),
            "Claude Code failed: API error"
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
            exited: true,
        };
        assert!(state.is_error);
        assert!(run.is_missing_conversation("x"));
        assert!(!run.is_missing_conversation("other"));
        let worked = ClaudeRun::Failed {
            code: Some(1),
            errors: state.errors,
            stderr_tail: vec![],
            did_work: true,
            exited: true,
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
        let ids: Vec<&str> = prompt.options.iter().map(|o| o.id.as_str()).collect();
        assert_eq!(ids, ["allow", "allow_session", "allow_always", "deny"]);
        assert!(prompt.diff.is_none());
        assert_eq!(prompt.auto, AutoAllow::Never);

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
        assert_eq!(updates[0]["destination"], "localSettings");

        let session = claude_permission_response("r1", &request, Some("allow_session"));
        let updates = session["response"]["response"]["updatedPermissions"]
            .as_array()
            .unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0]["destination"], "session");

        let deny = claude_permission_response("r1", &request, Some("deny"));
        assert_eq!(deny["response"]["response"]["behavior"], "deny");
        assert!(deny["response"]["response"].get("interrupt").is_none());
        let stopped = claude_permission_response("r1", &request, None);
        assert_eq!(stopped["response"]["response"]["interrupt"], true);

        let edit = json!({"tool_name": "Edit", "input": {
            "file_path": "x", "old_string": "a\n", "new_string": "b\n"
        }});
        let prompt = claude_permission_prompt(&edit);
        let kinds: Vec<String> = prompt.options.into_iter().map(|o| o.kind).collect();
        assert_eq!(kinds, ["allow_once", "reject_once"]);
        let diff = prompt.diff.unwrap();
        assert!(diff.contains("-a") && diff.contains("+b"));
        // The diff replaces the raw input in the description.
        assert!(!prompt.description.contains("old_string"));
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
    fn questions_and_answers_round_trip() {
        let input = json!({"questions": [
            {"question": "Which color?", "header": "Color", "multiSelect": false,
             "options": [{"label": "Red", "description": "warm"}, {"label": "Blue", "description": "cool"}]},
            {"question": "Which sizes?", "header": "", "multiSelect": true,
             "options": [{"label": "S", "description": ""}, {"label": "M", "description": ""}]},
            {"question": "  ", "options": []}
        ]});
        let qs = claude_questions(&input);
        assert_eq!(qs.len(), 2);
        assert_eq!(qs[0].id, "Which color?");
        assert_eq!(qs[0].options[1].label, "Blue");
        assert_eq!(qs[1].header, "Question 2");
        assert!(qs[1].multi_select && qs[1].allow_other);

        let resp = claude_answers_response(
            &input,
            &qs,
            Some(vec![vec!["Blue".into()], vec!["S".into(), "M".into()]]),
        );
        assert_eq!(resp["behavior"], "allow");
        assert_eq!(resp["updatedInput"]["answers"]["Which color?"], "Blue");
        assert_eq!(resp["updatedInput"]["answers"]["Which sizes?"], "S, M");
        assert_eq!(resp["updatedInput"]["questions"], input["questions"]);

        let dismissed = claude_answers_response(&input, &qs, None);
        assert_eq!(dismissed["behavior"], "deny");
    }

    #[test]
    fn permission_modes_map_to_claude_flags() {
        assert_eq!(claude_permission_mode(ApprovalMode::Plan), "plan");
        assert_eq!(claude_permission_mode(ApprovalMode::Ask), "default");
        assert_eq!(
            claude_permission_mode(ApprovalMode::AcceptEdits),
            "acceptEdits"
        );
        assert_eq!(
            claude_permission_mode(ApprovalMode::FullAccess),
            "bypassPermissions"
        );
    }
}
