use crate::adapters::{
    claude_prompt_with_attachments, codex_prompt_with_attachments, is_image_path,
};
use crate::claude::{run_claude_turn, ClaudeControls, ClaudePool, ClaudeTurn};
use crate::codex::{run_codex_turn, CodexPool, CodexTurnRequest};
use crate::diff;
use crate::permissions::{mode_of, Answers, AutoAllow, InputResponders, ModeStore, PermissionGate};
use crate::process::IDLE_TIMEOUT;
use anyhow::{Context, Result};
use same_acp::exec::{
    self, build_command, build_console_command, resolve_target, ConsoleScript, ExecPreference,
    ExecTarget,
};
use same_acp::{
    AcpClient, AcpClientConfig, AcpEvent, AcpModelState, PermissionDecision, PermissionResponder,
    SessionUpdate,
};
use same_core::{
    ApprovalMode, EventBus, ModelInfo, PermissionOption, PermissionRequest, ProviderCapabilities,
    ProviderConfig, ProviderKind, SameEvent, SessionStatus, SlashCommandInfo, ToolCallRecord,
    ToolCallStatus, MAX_CONCURRENT_SESSIONS, MIN_CONCURRENT_SESSIONS,
};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{oneshot, Mutex};

/// Deferred skill staging & explicit skill activation to execute inside
/// [`AgentManager::start_prompt`] after cancellation is registered and the
/// actual [`ExecTarget`] is resolved.
#[derive(Clone)]
pub struct PromptSkillSync {
    pub synchronizer: crate::skills::SkillSynchronizer,
    pub project_id: Option<String>,
    pub resolution: same_core::SkillResolution,
    /// Explicitly invoked skills `(skill_name, host_skill_md_path)`.
    pub explicit_skills: Vec<(String, PathBuf)>,
}

impl std::fmt::Debug for PromptSkillSync {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PromptSkillSync")
            .field("project_id", &self.project_id)
            .field("explicit_skills", &self.explicit_skills)
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct PromptRequest {
    pub session_id: String,
    pub external_session_id: Option<String>,
    pub provider: ProviderConfig,
    pub model: Option<String>,
    /// Reasoning effort (e.g. "high"); `None` leaves it to the agent.
    pub effort: Option<String>,
    pub cwd: PathBuf,
    pub prompt: String,
    /// Files attached to the prompt, as host paths. Each agent kind gets
    /// them in its own form, converted to the paths its target sees.
    pub attachments: Vec<PathBuf>,
    /// Where the project's agents should run (see [`ExecPreference`]).
    pub exec: ExecPreference,
    /// The session's approval mode.
    pub mode: ApprovalMode,
    /// When true, the session is restricted to read-only operations.
    pub read_only: bool,
    /// Optional handoff-enriched prompt used automatically if resuming a stored
    /// native provider session fails and falls back to a brand-new session.
    pub fallback_handoff_prompt: Option<String>,
    /// Optional SAME-managed skill delivery plan prepared for this turn.
    pub skill_delivery: Option<crate::skills::SkillDeliveryPlan>,
    /// Optional deferred skill sync + explicit skill formatting performed
    /// inside `start_prompt` under the session's cancellation token.
    pub skill_sync: Option<PromptSkillSync>,
}

/// A one-off setup step for a provider's CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderSetup {
    InstallCli,
    Login,
}

#[derive(Debug, Clone)]
pub struct SetupOutcome {
    pub message: String,
    /// Absolute path of a freshly installed CLI the app should use, when
    /// it is not on the app's PATH yet.
    pub installed_command: Option<String>,
}

/// Live events from an in-app interactive login process (e.g. `agy acp --login`).
#[derive(Debug, Clone)]
pub enum InteractiveLoginEvent {
    Output(String),
    Done(Result<String, String>),
}

/// Controls a running in-app interactive login process.
pub struct InteractiveLoginHandle {
    stdin_tx: tokio::sync::mpsc::UnboundedSender<String>,
    cancel_tx: std::sync::Mutex<Option<oneshot::Sender<()>>>,
}

impl InteractiveLoginHandle {
    /// Sends one line (e.g. an authorization code or redirect URL) to the
    /// child process's standard input.
    pub fn send_line(&self, line: String) -> bool {
        self.stdin_tx.send(line).is_ok()
    }

    /// Terminates the running login process.
    pub fn cancel(&self) {
        if let Some(tx) = self.cancel_tx.lock().unwrap().take() {
            let _ = tx.send(());
        }
    }
}

/// Drains all complete UTF-8 sequences (and any invalid bytes) from `pending`,
/// leaving only an incomplete trailing multi-byte UTF-8 sequence for the next
/// read.
fn drain_utf8_chunks(pending: &mut Vec<u8>) -> String {
    let mut out = String::new();
    let mut pos = 0;
    while pos < pending.len() {
        match std::str::from_utf8(&pending[pos..]) {
            Ok(valid) => {
                out.push_str(valid);
                pos = pending.len();
                break;
            }
            Err(err) => {
                let valid_len = err.valid_up_to();
                if valid_len > 0 {
                    out.push_str(
                        std::str::from_utf8(&pending[pos..pos + valid_len]).unwrap_or_default(),
                    );
                    pos += valid_len;
                }
                match err.error_len() {
                    Some(invalid_len) => {
                        out.push('\u{FFFD}');
                        pos += invalid_len;
                    }
                    None => break,
                }
            }
        }
    }
    pending.drain(..pos);
    out
}

fn has_dollar_skill_token(prompt: &str, name: &str) -> bool {
    let bytes = prompt.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' {
            let preceded_ok = i == 0
                || prompt[..i]
                    .chars()
                    .next_back()
                    .is_some_and(char::is_whitespace);
            if preceded_ok {
                let start = i + 1;
                let mut end = start;
                while end < bytes.len() {
                    let b = bytes[end];
                    if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' {
                        end += 1;
                    } else {
                        break;
                    }
                }
                if end > start && prompt[start..end].eq_ignore_ascii_case(name) {
                    return true;
                }
                i = end;
                continue;
            }
        }
        i += 1;
    }
    false
}

fn format_explicit_skills_prompt(
    prompt: &str,
    skills: &[(String, PathBuf)],
    provider: &ProviderConfig,
    target: &ExecTarget,
) -> String {
    if skills.is_empty() {
        return prompt.to_string();
    }
    match provider.kind {
        ProviderKind::CodexCli => {
            let missing: Vec<String> = skills
                .iter()
                .filter(|(name, _)| !has_dollar_skill_token(prompt, name))
                .map(|(name, _)| format!("${name}"))
                .collect();
            if missing.is_empty() {
                prompt.to_string()
            } else if prompt.trim().is_empty() {
                missing.join(" ")
            } else {
                format!("{}\n\n{prompt}", missing.join(" "))
            }
        }
        ProviderKind::ClaudeCli | ProviderKind::Acp => {
            let lines: Vec<String> = skills
                .iter()
                .map(|(name, md_path)| {
                    let target_path = target.to_target_path(md_path);
                    format!("- `{name}` ({target_path})")
                })
                .collect();
            let header = format!(
                "[Explicitly requested skills — read and follow their SKILL.md instructions:]\n{}",
                lines.join("\n")
            );
            if prompt.trim().is_empty() {
                header
            } else {
                format!("{header}\n\n{prompt}")
            }
        }
    }
}

/// `C:\x\agy.exe` or `/usr/bin/agy` -> `agy`.
fn command_name(command: &str) -> String {
    let base = command.rsplit(['/', '\\']).next().unwrap_or(command);
    base.strip_suffix(".exe")
        .or_else(|| base.strip_suffix(".cmd"))
        .unwrap_or(base)
        .to_ascii_lowercase()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderAuthStatus {
    LoggedIn,
    NotLoggedIn,
    NoSubscription,
    Unknown,
}

impl ProviderAuthStatus {
    /// Whether this auth status allows the provider to be used in workspaces.
    pub fn is_usable(self) -> bool {
        !matches!(self, Self::NotLoggedIn | Self::NoSubscription)
    }
}

/// Returns `true` when an error message from Claude Code indicates that the
/// logged-in Claude.ai account does not have an active subscription (e.g.
/// expired/past-due Pro/Max plan or free-tier account).
pub fn is_claude_subscription_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("oauth_not_allowed_for_organization")
        || lower.contains("can't use your claude subscription")
        || lower.contains("claude max or pro is required")
        || lower.contains("claude pro or max is required")
        || (lower.contains("requires a claude") && lower.contains("subscription"))
        || (lower.contains("no active") && lower.contains("subscription"))
}

#[derive(Debug, Clone)]
pub struct ProviderCheckResult {
    pub provider_id: String,
    pub available: bool,
    pub auth_status: ProviderAuthStatus,
    pub detail: String,
    pub capabilities: ProviderCapabilities,
    /// Where the check ran, e.g. "Windows" or "WSL: Ubuntu".
    pub target: String,
}

struct AcpSessionHandle {
    generation: u64,
    provider_id: String,
    target: ExecTarget,
    cwd: PathBuf,
    read_only: bool,
    /// Skill launch args/env used when this ACP process was spawned; if they
    /// change between turns, the pooled process is replaced so the agent sees
    /// the updated skill roots.
    skill_args: Vec<String>,
    skill_env: std::collections::BTreeMap<String, String>,
    client: Arc<AcpClient>,
    acp_session_id: String,
    /// The models the agent offers and the one in use; also updated by the
    /// agent's `config_option_update` notifications.
    model_state: SharedModelState,
    /// The agent's `modes` (`availableModes` / `currentModeId`), if any.
    modes: Option<Value>,
    /// The last confirmed ACP mode id on this session.
    current_mode_id: Arc<std::sync::Mutex<Option<String>>>,
    /// Last time this ACP session was used (for idle reaping).
    last_used: Arc<std::sync::Mutex<Instant>>,
}

type SharedModelState = Arc<Mutex<Option<AcpModelState>>>;

fn models_event(session_id: &str, provider_id: &str, state: &AcpModelState) -> SameEvent {
    SameEvent::ModelsUpdated {
        session_id: session_id.to_string(),
        provider_id: provider_id.to_string(),
        models: state
            .models
            .iter()
            .map(|m| ModelInfo {
                id: m.id.clone(),
                name: m.name.clone(),
                description: m.description.clone(),
                // ACP agents report one effort list for the session's model.
                efforts: state.efforts.iter().map(|e| e.id.clone()).collect(),
                default_effort: None,
            })
            .collect(),
        current: state.current.clone(),
    }
}

#[derive(Clone)]
pub struct AgentManager {
    events: EventBus,
    /// Runtime used for work spawned from synchronous entry points (e.g. the UI thread),
    /// which do not run inside a Tokio context.
    runtime: tokio::runtime::Handle,
    acp_sessions: Arc<Mutex<HashMap<String, AcpSessionHandle>>>,
    /// Monotonic generation counter so an async `release_session` never shuts
    /// down a replacement ACP client created after the release was requested.
    acp_generation: Arc<AtomicU64>,
    /// Pending ACP permission requests per SAME session: `(client_ptr_addr, request_id)`.
    acp_pending_permissions: Arc<Mutex<HashMap<String, Vec<(usize, String)>>>>,
    cancellers: Arc<Mutex<HashMap<String, oneshot::Sender<()>>>>,
    permission_responders: Arc<Mutex<HashMap<String, PermissionResponder>>>,
    /// Per ACP provider: the model and effort a fresh session starts with,
    /// restored when a session goes back to "Default".
    acp_defaults: Arc<Mutex<HashMap<String, AcpDefaults>>>,
    input_responders: InputResponders,
    /// Each session's approval mode, read live by prompts.
    modes: ModeStore,
    gate: PermissionGate,
    claude_pool: Arc<ClaudePool>,
    claude_controls: ClaudeControls,
    codex_pool: Arc<CodexPool>,
    /// Currently executing SAME session IDs (prevents duplicate runs & tracks concurrency).
    active_sessions: Arc<std::sync::Mutex<HashSet<String>>>,
    /// Sessions locked to Read Only mode; live approval-mode changes cannot elevate them.
    read_only_sessions: Arc<std::sync::Mutex<HashSet<String>>>,
    max_concurrent_sessions: Arc<AtomicUsize>,
}

struct ActiveRunGuard {
    active_sessions: Arc<std::sync::Mutex<HashSet<String>>>,
    session_id: String,
}

impl Drop for ActiveRunGuard {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.active_sessions.lock() {
            guard.remove(&self.session_id);
        }
    }
}

#[derive(Debug, Clone, Default)]
struct AcpDefaults {
    model: Option<String>,
    /// As the agent reports it; may be "default".
    effort: Option<String>,
}

impl AgentManager {
    pub fn new(events: EventBus, runtime: tokio::runtime::Handle) -> Self {
        let permission_responders = Arc::new(Mutex::new(HashMap::new()));
        let input_responders = InputResponders::default();
        let modes = ModeStore::default();
        let gate = PermissionGate::new(
            events.clone(),
            permission_responders.clone(),
            input_responders.clone(),
            modes.clone(),
        );
        let manager = Self {
            events,
            runtime,
            acp_sessions: Arc::new(Mutex::new(HashMap::new())),
            acp_generation: Arc::new(AtomicU64::new(1)),
            acp_pending_permissions: Arc::new(Mutex::new(HashMap::new())),
            cancellers: Arc::new(Mutex::new(HashMap::new())),
            permission_responders,
            acp_defaults: Arc::new(Mutex::new(HashMap::new())),
            input_responders,
            modes,
            gate,
            claude_pool: Arc::new(ClaudePool::default()),
            claude_controls: ClaudeControls::default(),
            codex_pool: Arc::new(CodexPool::default()),
            active_sessions: Arc::new(std::sync::Mutex::new(HashSet::new())),
            read_only_sessions: Arc::new(std::sync::Mutex::new(HashSet::new())),
            // 0 = unlimited (default).
            max_concurrent_sessions: Arc::new(AtomicUsize::new(0)),
        };
        // Stops CLI and ACP agent processes left idle.
        {
            let claude = Arc::downgrade(&manager.claude_pool);
            let codex = Arc::downgrade(&manager.codex_pool);
            let acp = Arc::downgrade(&manager.acp_sessions);
            let active = Arc::downgrade(&manager.active_sessions);
            manager.runtime.spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
                loop {
                    tick.tick().await;
                    let (Some(claude), Some(codex), Some(acp), Some(active)) = (
                        claude.upgrade(),
                        codex.upgrade(),
                        acp.upgrade(),
                        active.upgrade(),
                    ) else {
                        break;
                    };
                    claude.reap_idle(IDLE_TIMEOUT).await;
                    codex.reap_idle(IDLE_TIMEOUT).await;
                    let stale_clients: Vec<Arc<AcpClient>> = {
                        let mut guard = acp.lock().await;
                        // Check `active` *after* acquiring `acp` lock so a newly
                        // admitted turn cannot race with idle eviction.
                        let active_set = active.lock().unwrap().clone();
                        let stale_ids: Vec<String> = guard
                            .iter()
                            .filter(|(sid, handle)| {
                                !active_set.contains(*sid)
                                    && handle
                                        .last_used
                                        .lock()
                                        .map(|t| t.elapsed() >= IDLE_TIMEOUT)
                                        .unwrap_or(false)
                            })
                            .map(|(sid, _)| sid.clone())
                            .collect();
                        stale_ids
                            .into_iter()
                            .filter_map(|sid| guard.remove(&sid).map(|h| h.client))
                            .collect()
                    };
                    for client in stale_clients {
                        client.shutdown().await;
                    }
                }
            });
        }
        manager
    }

    pub fn set_max_concurrent_sessions(&self, limit: Option<usize>) {
        let raw = limit
            .filter(|&n| n > 0)
            .map(|n| n.clamp(MIN_CONCURRENT_SESSIONS, MAX_CONCURRENT_SESSIONS))
            .unwrap_or(0);
        self.max_concurrent_sessions.store(raw, Ordering::Relaxed);
    }

    pub fn max_concurrent_sessions(&self) -> Option<usize> {
        let raw = self.max_concurrent_sessions.load(Ordering::Relaxed);
        (raw > 0).then_some(raw)
    }

    pub fn active_session_count(&self) -> usize {
        self.active_sessions.lock().unwrap().len()
    }

    pub fn is_session_running(&self, session_id: &str) -> bool {
        self.active_sessions.lock().unwrap().contains(session_id)
    }

    /// Checks whether a new prompt turn can be started for `session_id` without
    /// mutating state (duplicate-run and concurrency-limit preflight).
    pub fn can_start_prompt(&self, session_id: &str) -> Result<()> {
        let active = self.active_sessions.lock().unwrap();
        if active.contains(session_id) {
            anyhow::bail!("Session '{session_id}' is already running");
        }
        if let Some(limit) = self.max_concurrent_sessions() {
            if active.len() >= limit {
                anyhow::bail!(
                    "Concurrent session limit reached ({}/{}). Wait for an active session to finish or raise the limit in settings.",
                    active.len(),
                    limit
                );
            }
        }
        Ok(())
    }

    /// Changes a session's approval mode, also for a turn already running:
    /// prompts read it live, Claude Code switches its permission mode, and an
    /// ACP agent with matching modes switches too. Codex picks it up at its
    /// next turn.
    pub fn set_session_mode(&self, session_id: &str, mode: ApprovalMode) {
        let mode = if self.read_only_sessions.lock().unwrap().contains(session_id) {
            ApprovalMode::Plan
        } else {
            mode
        };
        let previous = self
            .modes
            .lock()
            .unwrap()
            .insert(session_id.to_string(), mode);
        if previous == Some(mode) {
            return;
        }
        let controls = self.claude_controls.clone();
        let acp_sessions = self.acp_sessions.clone();
        let modes_map = self.modes.clone();
        let events = self.events.clone();
        let sid = session_id.to_string();
        self.runtime.spawn(async move {
            // Re-read the latest requested mode so rapid switches cannot apply
            // an older selection out of order.
            let latest = mode_of(&modes_map, &sid);
            controls.set_mode(&sid, latest).await;
            let handle = acp_sessions.lock().await.get(&sid).map(|h| {
                (
                    h.client.clone(),
                    h.acp_session_id.clone(),
                    h.provider_id.clone(),
                    h.modes.clone(),
                    h.current_mode_id.clone(),
                )
            });
            if let Some((client, acp_sid, provider_id, modes, current_mode_id)) = handle {
                let latest = mode_of(&modes_map, &sid);
                let failed = match modes {
                    Some(modes) => match acp_mode_id(&modes, latest) {
                        Some(id) => match set_acp_mode_bounded(&client, &acp_sid, &id).await {
                            Ok(()) => {
                                *current_mode_id.lock().unwrap() = Some(id);
                                None
                            }
                            Err(e) => Some(format!("{e:#}")),
                        },
                        None if latest != ApprovalMode::FullAccess => Some(format!(
                            "agent does not advertise a mode for '{}'",
                            latest.as_str()
                        )),
                        None => None,
                    },
                    None if latest == ApprovalMode::Plan => {
                        Some("agent does not advertise a Plan (read-only) mode".to_string())
                    }
                    None => None,
                };
                if let Some(reason) = failed {
                    events.publish(SameEvent::AgentLog {
                        session_id: Some(sid.clone()),
                        provider_id,
                        line: format!(
                            "Failed to switch ACP session mode to '{}': {reason}",
                            latest.as_str()
                        ),
                    });
                    if latest != ApprovalMode::FullAccess {
                        let _ = client.cancel(&acp_sid).await;
                    }
                }
            }
        });
    }

    /// Stops a session's agent processes (CLI and ACP) and cleans up its state
    /// when the session is deleted or moved to another provider.
    pub fn release_session(&self, session_id: &str) {
        self.claude_controls.remove(session_id);
        self.modes.lock().unwrap().remove(session_id);
        self.read_only_sessions.lock().unwrap().remove(session_id);
        let cutoff_gen = self.acp_generation.fetch_add(1, Ordering::SeqCst);
        let claude = self.claude_pool.clone();
        let codex = self.codex_pool.clone();
        let acp_sessions = self.acp_sessions.clone();
        let manager = self.clone();
        let sid = session_id.to_string();
        self.runtime.spawn(async move {
            let stale_handle = {
                let mut guard = acp_sessions.lock().await;
                if guard.get(&sid).is_some_and(|h| h.generation <= cutoff_gen) {
                    guard.remove(&sid)
                } else {
                    None
                }
            };
            if let Some(handle) = stale_handle {
                manager.withdraw_acp_permissions(&sid).await;
                handle.client.shutdown().await;
            }
            claude.release(&sid).await;
            codex.release(&sid).await;
        });
    }

    /// Answers a question card; `None` dismisses it.
    pub async fn respond_user_input(&self, request_id: &str, answers: Answers) {
        if let Some(tx) = self.input_responders.lock().await.remove(request_id) {
            let _ = tx.send(answers);
        }
    }

    pub fn capabilities_for(kind: ProviderKind) -> ProviderCapabilities {
        match kind {
            ProviderKind::Acp => ProviderCapabilities {
                supports_acp: true,
                supports_streaming: true,
                supports_thinking: true,
                supports_tool_calls: true,
                supports_permissions: true,
                supports_cancel: true,
                supports_session_resume: true,
                supports_model_select: true,
                supports_slash_commands: true,
                supports_read_only: true,
            },
            ProviderKind::ClaudeCli => ProviderCapabilities {
                supports_acp: false,
                supports_streaming: true,
                supports_thinking: true,
                supports_tool_calls: true,
                supports_permissions: true,
                supports_cancel: true,
                supports_session_resume: true,
                supports_model_select: true,
                supports_slash_commands: false,
                supports_read_only: true,
            },
            ProviderKind::CodexCli => ProviderCapabilities {
                supports_acp: false,
                supports_streaming: true,
                supports_thinking: true,
                supports_tool_calls: true,
                supports_permissions: true,
                supports_cancel: true,
                supports_session_resume: true,
                supports_model_select: true,
                supports_slash_commands: false,
                supports_read_only: true,
            },
        }
    }

    /// Checks that the provider's command runs where the project at `cwd`
    /// would run it (natively, or inside WSL).
    pub async fn check_provider(
        &self,
        provider: &ProviderConfig,
        exec: ExecPreference,
        cwd: &Path,
    ) -> ProviderCheckResult {
        let caps = Self::capabilities_for(provider.kind);
        let target = resolve_target(exec, cwd).await;
        let mut cmd = build_command(
            &target,
            &provider.command,
            &["--version".to_string()],
            cwd,
            &provider.env,
        )
        .await;
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let output = tokio::time::timeout(std::time::Duration::from_secs(30), cmd.output())
            .await
            .unwrap_or_else(|_| Err(std::io::Error::other("timed out")));

        match output {
            Ok(out) => {
                let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
                let err_text = String::from_utf8_lossy(&out.stderr).trim().to_string();
                let detail = if !text.is_empty() {
                    text
                } else if !err_text.is_empty() {
                    err_text
                } else {
                    format!("'{}' found", provider.command)
                };
                let available = out.status.success();
                // `env` exits 127 when the command is not installed in WSL.
                let missing_in_wsl =
                    matches!(target, ExecTarget::Wsl { .. }) && out.status.code() == Some(127);
                let auth_status = if available {
                    Self::check_auth_status(provider, &target, cwd).await
                } else {
                    ProviderAuthStatus::Unknown
                };
                ProviderCheckResult {
                    provider_id: provider.id.clone(),
                    available,
                    auth_status,
                    detail: if available {
                        detail
                    } else if missing_in_wsl {
                        format!(
                            "Not installed in {}: '{}' (install it inside WSL, or switch this project to Windows)",
                            target.label(),
                            provider.command
                        )
                    } else {
                        format!("'{}' failed: {detail}", provider.command)
                    },
                    capabilities: caps,
                    target: target.label(),
                }
            }
            Err(e) => ProviderCheckResult {
                provider_id: provider.id.clone(),
                available: false,
                auth_status: ProviderAuthStatus::Unknown,
                detail: format!("Not found ({}): {e}", provider.command),
                capabilities: caps,
                target: target.label(),
            },
        }
    }

    async fn check_auth_status(
        provider: &ProviderConfig,
        target: &ExecTarget,
        cwd: &Path,
    ) -> ProviderAuthStatus {
        let base = command_name(&provider.command);
        if base == "claude" && has_claude_credential_arg_override(&provider.args) {
            // `claude auth status` does not accept prompt-time credential flags
            // such as `--settings` (e.g. `apiKeyHelper`) or `--bare`, so probing
            // default auth could falsely hide a working provider.
            return ProviderAuthStatus::Unknown;
        }
        let args: &[&str] = match base.as_str() {
            "claude" => &["auth", "status"],
            "codex" => &["login", "status"],
            "opencode" => &["auth", "list"],
            _ => return ProviderAuthStatus::Unknown,
        };
        let owned_args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let mut cmd =
            build_command(target, &provider.command, &owned_args, cwd, &provider.env).await;
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let output = tokio::time::timeout(std::time::Duration::from_secs(10), cmd.output())
            .await
            .unwrap_or_else(|_| Err(std::io::Error::other("timed out")));
        let Ok(out) = output else {
            return ProviderAuthStatus::Unknown;
        };
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        match base.as_str() {
            "claude" => {
                let status = parse_claude_auth_status(out.status.success(), &stdout);
                if matches!(
                    status,
                    ProviderAuthStatus::LoggedIn | ProviderAuthStatus::NoSubscription
                ) {
                    Self::check_claude_subscription_state(&stdout, status, target, cwd).await
                } else {
                    status
                }
            }
            "codex" => {
                let status = parse_codex_auth_status(out.status.success(), &stdout, &stderr);
                if status == ProviderAuthStatus::NotLoggedIn
                    && Self::codex_may_use_runtime_credentials(provider, target, cwd).await
                {
                    ProviderAuthStatus::Unknown
                } else {
                    status
                }
            }
            "opencode" => parse_opencode_auth_status(out.status.success(), &stdout, &stderr),
            _ => ProviderAuthStatus::Unknown,
        }
    }

    /// Returns `true` when Codex may authenticate at runtime via environment
    /// API keys (`OPENAI_API_KEY` / `CODEX_API_KEY`) or CLI config/profile
    /// overrides (`-c`/`--config`, `-p`/`--profile`), which `codex login status`
    /// does not treat as a stored login.
    async fn codex_may_use_runtime_credentials(
        provider: &ProviderConfig,
        target: &ExecTarget,
        cwd: &Path,
    ) -> bool {
        if has_codex_runtime_auth_override(provider) {
            return true;
        }
        match target {
            ExecTarget::Host => ["OPENAI_API_KEY", "CODEX_API_KEY"]
                .iter()
                .any(|k| std::env::var(k).ok().is_some_and(|v| !v.trim().is_empty())),
            ExecTarget::Wsl { .. } => {
                let mut cmd = build_command(
                    target,
                    "sh",
                    &[
                        "-c".to_string(),
                        r#"test -n "${OPENAI_API_KEY:-}${CODEX_API_KEY:-}""#.to_string(),
                    ],
                    cwd,
                    &provider.env,
                )
                .await;
                cmd.stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .kill_on_drop(true);
                tokio::time::timeout(std::time::Duration::from_secs(5), cmd.status())
                    .await
                    .ok()
                    .and_then(|res| res.ok())
                    .is_some_and(|s| s.success())
            }
        }
    }

    /// Refines a first-party `claude.ai` auth status using Claude's local state
    /// file (`.claude.json`).
    ///
    /// - When `claude auth status` reports a stale cached paid plan (e.g.
    ///   `"pro"`), `.claude.json`'s `oauthAccount` can show `billingType: "none"`
    ///   after the subscription lapses (`LoggedIn` -> `NoSubscription`).
    /// - When `claude auth status` reports no paid plan (`subscriptionType: null`),
    ///   `.claude.json` can confirm active extra usage or an active trial
    ///   (`NoSubscription` -> `LoggedIn`).
    async fn check_claude_subscription_state(
        auth_stdout: &str,
        base_status: ProviderAuthStatus,
        target: &ExecTarget,
        cwd: &Path,
    ) -> ProviderAuthStatus {
        let Ok(status_val) = serde_json::from_str::<Value>(auth_stdout.trim()) else {
            return base_status;
        };
        if !requires_claude_subscription(&status_val) {
            return base_status;
        }
        let Some(config_dir) = status_val
            .get("configDirectory")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
        else {
            return base_status;
        };
        let host_paths: Vec<PathBuf> = claude_state_file_candidates(config_dir)
            .into_iter()
            .map(|p| target.to_host_path(cwd, &p))
            .collect();
        let state_val = tokio::task::spawn_blocking(move || {
            for path in host_paths {
                if let Ok(raw) = std::fs::read_to_string(&path) {
                    if let Ok(val) = serde_json::from_str::<Value>(&raw) {
                        return Some(val);
                    }
                }
            }
            None
        })
        .await
        .ok()
        .flatten();
        match state_val {
            Some(val) => {
                refine_claude_auth_with_state(&status_val, &val, base_status, chrono::Utc::now())
            }
            None => base_status,
        }
    }

    /// Asks the provider's agent which models it offers. Returns where it ran
    /// (e.g. "WSL: Ubuntu") with the list.
    pub async fn discover_models(
        &self,
        provider: &ProviderConfig,
        exec: ExecPreference,
        cwd: &Path,
    ) -> (String, Result<crate::discovery::Discovered>) {
        let target = resolve_target(exec, cwd).await;
        let res = crate::discovery::discover_models(provider, &target, cwd).await;
        if let Ok(found) = &res {
            if let Some(state) = &found.acp_defaults {
                self.record_acp_defaults(&provider.id, state).await;
            }
        }
        (target.label(), res)
    }

    /// Asks the provider's agent for the usage limits of its login. Returns
    /// where it ran (e.g. "WSL: Ubuntu") with the report.
    pub async fn fetch_quota(
        &self,
        provider: &ProviderConfig,
        exec: ExecPreference,
        cwd: &Path,
    ) -> (String, Result<same_core::QuotaReport>) {
        let target = resolve_target(exec, cwd).await;
        let res = crate::quota::fetch_quota(provider, &target, cwd).await;
        (target.label(), res)
    }

    /// Reads how many tokens the provider used in the last `days` days from
    /// its local history. Returns where it was read (e.g. "WSL: Ubuntu").
    pub async fn fetch_stats(
        &self,
        provider: &ProviderConfig,
        exec: ExecPreference,
        cwd: &Path,
        days: u32,
    ) -> (String, Result<same_core::StatsReport>) {
        let target = resolve_target(exec, cwd).await;
        let res = crate::stats::fetch_stats(provider, &target, cwd, days).await;
        (target.label(), res)
    }

    /// Remembers a fresh ACP session's model and effort as the agent's
    /// defaults (a new session never inherits another session's choice).
    async fn record_acp_defaults(&self, provider_id: &str, state: &AcpModelState) {
        if state.current.is_none() && state.current_effort.is_none() {
            return;
        }
        self.acp_defaults.lock().await.insert(
            provider_id.to_string(),
            AcpDefaults {
                model: state.current.clone(),
                effort: state.current_effort.clone(),
            },
        );
    }

    pub async fn test_acp_connection(
        &self,
        provider: &ProviderConfig,
        exec: ExecPreference,
        cwd: PathBuf,
    ) -> Result<String> {
        let target = resolve_target(exec, &cwd).await;
        let (client, _rx) = AcpClient::spawn(AcpClientConfig {
            command: provider.command.clone(),
            args: provider.args.clone(),
            env: provider.env.clone(),
            cwd,
            auto_approve: true,
            target,
        })
        .await?;

        let info = client
            .init_result
            .agent_info
            .as_ref()
            .map(|i| {
                format!(
                    "{} {}",
                    i.title.as_deref().unwrap_or(&i.name),
                    i.version.as_deref().unwrap_or("")
                )
                .trim()
                .to_string()
            })
            .unwrap_or_else(|| "ACP agent connected".to_string());

        let auth_note = if client.init_result.auth_methods.is_empty() {
            String::new()
        } else {
            let names: Vec<&str> = client
                .init_result
                .auth_methods
                .iter()
                .map(|m| m.name.as_str())
                .collect();
            format!(" | Auth: {}", names.join(", "))
        };

        client.shutdown().await;
        Ok(format!("{info}{auth_note}"))
    }

    /// The official installer for a provider's CLI, when SAME knows one.
    /// Only Antigravity is offered: its CLI is separate from the desktop app.
    pub fn install_script(provider: &ProviderConfig) -> Option<ConsoleScript> {
        let is_antigravity =
            provider.id == "antigravity" || command_name(&provider.command) == "agy";
        is_antigravity.then(|| ConsoleScript {
            unix: "curl -fsSL https://antigravity.google/cli/install.sh | bash".to_string(),
            windows: "irm https://antigravity.google/cli/install.ps1 | iex".to_string(),
        })
    }

    /// The sign-in command for a provider's CLI.
    pub fn login_script(provider: &ProviderConfig) -> ConsoleScript {
        let args: &[&str] = match command_name(&provider.command).as_str() {
            "claude" => &["auth", "login"],
            "codex" => &["login"],
            "opencode" => &["auth", "login"],
            // Antigravity CLI docs: `agy acp --login` signs in for ACP use.
            "agy" => &["acp", "--login"],
            _ => &["login"],
        };
        ConsoleScript::program(&provider.command, args)
    }

    /// Whether this provider's sign-in can run inside SAME (piped stdin/stdout)
    /// without opening an external terminal window.
    pub fn supports_in_app_login(provider: &ProviderConfig) -> bool {
        provider.id == "antigravity" || command_name(&provider.command) == "agy"
    }

    /// Runs a provider's interactive sign-in command inside SAME with piped
    /// stdin/stdout/stderr, streaming output chunks and accepting lines (such
    /// as an OAuth authorization code or redirect URL) via the returned handle.
    pub fn start_in_app_login(
        &self,
        provider: ProviderConfig,
        exec: ExecPreference,
        cwd: PathBuf,
        on_event: impl Fn(InteractiveLoginEvent) + Send + Sync + 'static,
    ) -> InteractiveLoginHandle {
        let (stdin_tx, mut stdin_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let (cancel_tx, mut cancel_rx) = oneshot::channel::<()>();
        let on_event = Arc::new(on_event);

        self.runtime.spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};

            let args: Vec<String> = match command_name(&provider.command).as_str() {
                "claude" => vec!["auth".into(), "login".into()],
                "codex" => vec!["login".into()],
                "opencode" => vec!["auth".into(), "login".into()],
                "agy" => vec!["acp".into(), "--login".into()],
                _ => vec!["login".into()],
            };

            let target = resolve_target(exec, &cwd).await;
            let mut cmd =
                build_command(&target, &provider.command, &args, &cwd, &provider.env).await;
            cmd.stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);

            let mut child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => {
                    on_event(InteractiveLoginEvent::Done(Err(format!(
                        "Failed to start {}: {e}",
                        provider.command
                    ))));
                    return;
                }
            };

            let mut stdin = child.stdin.take();
            let stdout = child.stdout.take();
            let stderr = child.stderr.take();

            let writer = tokio::spawn(async move {
                if let Some(mut stdin) = stdin.take() {
                    while let Some(line) = stdin_rx.recv().await {
                        let payload = format!("{}\n", line.trim());
                        if stdin.write_all(payload.as_bytes()).await.is_err() {
                            break;
                        }
                        if stdin.flush().await.is_err() {
                            break;
                        }
                    }
                }
            });

            let out_cb = on_event.clone();
            let mut out_reader = tokio::spawn(async move {
                if let Some(mut r) = stdout {
                    let mut buf = [0u8; 1024];
                    let mut pending = Vec::new();
                    while let Ok(n) = r.read(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                        pending.extend_from_slice(&buf[..n]);
                        let chunk = drain_utf8_chunks(&mut pending);
                        if !chunk.is_empty() {
                            out_cb(InteractiveLoginEvent::Output(chunk));
                        }
                    }
                    if !pending.is_empty() {
                        out_cb(InteractiveLoginEvent::Output(
                            String::from_utf8_lossy(&pending).into_owned(),
                        ));
                    }
                }
            });

            let err_cb = on_event.clone();
            let mut err_reader = tokio::spawn(async move {
                if let Some(mut r) = stderr {
                    let mut buf = [0u8; 1024];
                    let mut pending = Vec::new();
                    while let Ok(n) = r.read(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                        pending.extend_from_slice(&buf[..n]);
                        let chunk = drain_utf8_chunks(&mut pending);
                        if !chunk.is_empty() {
                            err_cb(InteractiveLoginEvent::Output(chunk));
                        }
                    }
                    if !pending.is_empty() {
                        err_cb(InteractiveLoginEvent::Output(
                            String::from_utf8_lossy(&pending).into_owned(),
                        ));
                    }
                }
            });

            let outcome = tokio::select! {
                res = child.wait() => {
                    let _ = tokio::time::timeout(
                        std::time::Duration::from_secs(2),
                        async {
                            let _ = (&mut out_reader).await;
                            let _ = (&mut err_reader).await;
                        },
                    ).await;
                    match res {
                        Ok(status) if status.success() => Ok("exit 0".to_string()),
                        Ok(status) => Err(format!("exit {}", status.code().unwrap_or(-1))),
                        Err(e) => Err(format!("Process error: {e}")),
                    }
                }
                _ = &mut cancel_rx => {
                    let _ = child.kill().await;
                    Err("Cancelled".to_string())
                }
            };

            writer.abort();
            out_reader.abort();
            err_reader.abort();
            on_event(InteractiveLoginEvent::Done(outcome));
        });

        InteractiveLoginHandle {
            stdin_tx,
            cancel_tx: std::sync::Mutex::new(Some(cancel_tx)),
        }
    }

    /// Installs a provider's CLI or signs in to it, in a terminal window in
    /// the environment where the project's agents run (WSL or the host), so
    /// the user can follow along and answer prompts. Resolves when the
    /// window is closed.
    pub async fn run_provider_setup(
        &self,
        provider: &ProviderConfig,
        action: ProviderSetup,
        exec: ExecPreference,
        cwd: &Path,
    ) -> Result<SetupOutcome> {
        let script = match action {
            ProviderSetup::InstallCli => Self::install_script(provider)
                .ok_or_else(|| anyhow::anyhow!("No installer is known for {}", provider.name))?,
            ProviderSetup::Login => Self::login_script(provider),
        };
        let target = resolve_target(exec, cwd).await;
        let message = if cfg!(windows) {
            let mut cmd = build_console_command(&target, cwd, &script, true).await;
            for (k, v) in &provider.env {
                cmd.env(k, v);
            }
            let status = cmd
                .status()
                .await
                .with_context(|| format!("Failed to open a terminal for {}", target.label()))?;
            format!("Terminal closed (exit {})", status.code().unwrap_or(-1))
        } else if cfg!(target_os = "macos") {
            let code = exec::run_macos_console(cwd, &script, &provider.env, true)
                .await
                .with_context(|| format!("Failed to open a terminal for {}", target.label()))?;
            format!("Terminal closed (exit {code})")
        } else {
            let mut cmd = build_console_command(&target, cwd, &script, false).await;
            for (k, v) in &provider.env {
                cmd.env(k, v);
            }
            // No terminal to open: run it and report what it printed.
            cmd.stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let out = tokio::time::timeout(std::time::Duration::from_secs(300), cmd.output())
                .await
                .map_err(|_| anyhow::anyhow!("Timed out"))??;
            let text = [
                String::from_utf8_lossy(&out.stdout).trim().to_string(),
                String::from_utf8_lossy(&out.stderr).trim().to_string(),
            ]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
            format!("[exit {}] {text}", out.status.code().unwrap_or(-1))
        };

        let mut installed_command = None;
        if action == ProviderSetup::InstallCli {
            // The installer may have added a directory to the shell profile.
            exec::clear_path_cache();
            // On Windows the new directory isn't on this process's PATH until
            // restart; point the provider at the installed binary instead.
            if target == ExecTarget::Host && cfg!(windows) {
                if let Some(local) = std::env::var_os("LOCALAPPDATA") {
                    let exe = PathBuf::from(local).join(r"agy\bin\agy.exe");
                    if exe.exists() {
                        installed_command = Some(exe.to_string_lossy().to_string());
                    }
                }
            }
        }
        Ok(SetupOutcome {
            message,
            installed_command,
        })
    }

    /// Starts a prompt turn on the given session, rejecting duplicate runs on
    /// the same session or runs exceeding `max_concurrent_sessions`.
    pub fn try_start_prompt(&self, req: PromptRequest) -> Result<()> {
        {
            let mut active = self.active_sessions.lock().unwrap();
            if active.contains(&req.session_id) {
                anyhow::bail!("Session '{}' is already running", req.session_id);
            }
            if let Some(limit) = self.max_concurrent_sessions() {
                if active.len() >= limit {
                    anyhow::bail!(
                        "Concurrent session limit reached ({}/{}). Wait for an active session to finish or raise the limit in settings.",
                        active.len(),
                        limit
                    );
                }
            }
            active.insert(req.session_id.clone());
        }

        let guard = ActiveRunGuard {
            active_sessions: self.active_sessions.clone(),
            session_id: req.session_id.clone(),
        };

        {
            let mut ro = self.read_only_sessions.lock().unwrap();
            if req.read_only {
                ro.insert(req.session_id.clone());
            } else {
                ro.remove(&req.session_id);
            }
        }
        let effective_mode = if req.read_only {
            ApprovalMode::Plan
        } else {
            req.mode
        };
        let manager = self.clone();
        self.modes
            .lock()
            .unwrap()
            .insert(req.session_id.clone(), effective_mode);
        self.runtime.spawn(async move {
            let _run_guard = guard;
            let sid = req.session_id.clone();
            let pid = req.provider.id.clone();
            manager.events.publish(SameEvent::SessionStarted {
                session_id: sid.clone(),
                provider_id: pid.clone(),
            });
            manager.events.publish(SameEvent::SessionStatusChanged {
                session_id: sid.clone(),
                status: SessionStatus::Running,
            });

            let (cancel_tx, mut cancel_rx) = oneshot::channel();
            manager
                .cancellers
                .lock()
                .await
                .insert(sid.clone(), cancel_tx);

            let mut req = req;
            req.mode = effective_mode;
            let target = tokio::select! {
                _ = &mut cancel_rx => {
                    manager.cancellers.lock().await.remove(&sid);
                    manager.events.publish(SameEvent::SessionCancelled {
                        session_id: sid.clone(),
                    });
                    manager.events.publish(SameEvent::SessionStatusChanged {
                        session_id: sid,
                        status: SessionStatus::Cancelled,
                    });
                    return;
                }
                t = resolve_target(req.exec, &req.cwd) => t,
            };

            if let Some(sync_cfg) = req.skill_sync.take() {
                if !sync_cfg.explicit_skills.is_empty() {
                    req.prompt = format_explicit_skills_prompt(
                        &req.prompt,
                        &sync_cfg.explicit_skills,
                        &req.provider,
                        &target,
                    );
                    if let Some(fb) = req.fallback_handoff_prompt.take() {
                        req.fallback_handoff_prompt = Some(format_explicit_skills_prompt(
                            &fb,
                            &sync_cfg.explicit_skills,
                            &req.provider,
                            &target,
                        ));
                    }
                }
                let sync_provider = req.provider.clone();
                let sync_target = target.clone();
                let sync_fut = tokio::task::spawn_blocking(move || {
                    sync_cfg
                        .synchronizer
                        .sync_for_provider(
                            &sync_provider,
                            sync_cfg.project_id.as_deref(),
                            &sync_target,
                            &sync_cfg.resolution,
                        )
                        .ok()
                });
                let delivery = tokio::select! {
                    _ = &mut cancel_rx => {
                        manager.cancellers.lock().await.remove(&sid);
                        manager.events.publish(SameEvent::SessionCancelled {
                            session_id: sid.clone(),
                        });
                        manager.events.publish(SameEvent::SessionStatusChanged {
                            session_id: sid,
                            status: SessionStatus::Cancelled,
                        });
                        return;
                    }
                    res = sync_fut => res.ok().flatten(),
                };
                if req.skill_delivery.is_none() {
                    req.skill_delivery = delivery;
                }
            }
            let files_res: Result<Vec<String>> = req
                .attachments
                .iter()
                .map(|p| {
                    if p.to_str().is_none() {
                        anyhow::bail!(
                            "Attachment path is not valid UTF-8 and cannot be passed to the agent: {}",
                            p.display()
                        );
                    }
                    let translated = target.to_target_path(p);
                    if let ExecTarget::Wsl { distro } = &target {
                        if !translated.starts_with('/') {
                            anyhow::bail!(
                                "Attachment is not accessible from WSL distro '{distro}': {}",
                                p.display()
                            );
                        }
                    }
                    Ok(translated)
                })
                .collect();
            let res = match files_res {
                Err(err) => Err(err),
                Ok(files) => match req.provider.kind {
                    ProviderKind::Acp => {
                        manager.run_acp_prompt(req, target, files, cancel_rx).await
                    }
                    ProviderKind::ClaudeCli => {
                        let prompt = claude_prompt_with_attachments(&req.prompt, &files);
                        let fallback_prompt = req
                            .fallback_handoff_prompt
                            .as_deref()
                            .map(|fb| claude_prompt_with_attachments(fb, &files));
                        let skill_args = req
                            .skill_delivery
                            .as_ref()
                            .map(|d| d.extra_args.as_slice())
                            .unwrap_or(&[]);
                        let skill_hash = req
                            .skill_delivery
                            .as_ref()
                            .map(|d| d.deployment_hash.as_str())
                            .unwrap_or("");
                        run_claude_turn(
                            ClaudeTurn {
                                pool: &manager.claude_pool,
                                controls: &manager.claude_controls,
                                provider: &req.provider,
                                session_id: &req.session_id,
                                external_session_id: req.external_session_id.as_deref(),
                                model: req.model.as_deref(),
                                effort: req.effort.as_deref(),
                                cwd: &req.cwd,
                                target: &target,
                                prompt: &prompt,
                                fallback_handoff_prompt: fallback_prompt.as_deref(),
                                skill_args,
                                skill_hash,
                                gate: &manager.gate,
                                events: &manager.events,
                            },
                            cancel_rx,
                        )
                        .await
                    }
                    ProviderKind::CodexCli => {
                        let images: Vec<String> =
                            files.iter().filter(|f| is_image_path(f)).cloned().collect();
                        let prompt = codex_prompt_with_attachments(&req.prompt, &files);
                        let fallback_prompt = req
                            .fallback_handoff_prompt
                            .as_deref()
                            .map(|fb| codex_prompt_with_attachments(fb, &files));
                        let skill_roots = req
                            .skill_delivery
                            .as_ref()
                            .map(|d| d.codex_extra_roots.as_slice())
                            .unwrap_or(&[]);
                        let skill_hash = req
                            .skill_delivery
                            .as_ref()
                            .map(|d| d.deployment_hash.as_str())
                            .unwrap_or("");
                        run_codex_turn(
                            CodexTurnRequest {
                                pool: &manager.codex_pool,
                                provider: &req.provider,
                                session_id: &req.session_id,
                                external_session_id: req.external_session_id.as_deref(),
                                model: req.model.as_deref(),
                                effort: req.effort.as_deref(),
                                cwd: &req.cwd,
                                target: &target,
                                prompt: &prompt,
                                fallback_handoff_prompt: fallback_prompt.as_deref(),
                                read_only: req.read_only,
                                images: &images,
                                skill_roots,
                                skill_hash,
                                gate: &manager.gate,
                                events: &manager.events,
                            },
                            cancel_rx,
                        )
                        .await
                    }
                },
            };

            manager.cancellers.lock().await.remove(&sid);
            manager.withdraw_acp_permissions(&sid).await;

            if let Err(err) = res {
                manager.events.publish(SameEvent::SessionFailed {
                    session_id: sid.clone(),
                    error: format!("{err:#}"),
                });
                manager.events.publish(SameEvent::SessionStatusChanged {
                    session_id: sid,
                    status: SessionStatus::Failed,
                });
            }
        });
        Ok(())
    }

    pub fn start_prompt(&self, req: PromptRequest) {
        let sid = req.session_id.clone();
        if let Err(err) = self.try_start_prompt(req) {
            self.events.publish(SameEvent::SessionFailed {
                session_id: sid.clone(),
                error: format!("{err:#}"),
            });
            self.events.publish(SameEvent::SessionStatusChanged {
                session_id: sid,
                status: SessionStatus::Failed,
            });
        }
    }

    async fn run_acp_prompt(
        &self,
        req: PromptRequest,
        target: ExecTarget,
        files: Vec<String>,
        mut cancel_rx: oneshot::Receiver<()>,
    ) -> Result<()> {
        let (client, acp_session_id, model_state, fell_back) = tokio::select! {
            _ = &mut cancel_rx => {
                self.withdraw_acp_permissions(&req.session_id).await;
                self.events.publish(SameEvent::SessionCancelled {
                    session_id: req.session_id.clone(),
                });
                self.events.publish(SameEvent::SessionStatusChanged {
                    session_id: req.session_id,
                    status: SessionStatus::Cancelled,
                });
                return Ok(());
            }
            res = self.ensure_acp_session(&req, target) => res?,
        };
        self.apply_acp_model(&req, &client, &acp_session_id, &model_state)
            .await;
        self.apply_acp_effort(&req, &client, &acp_session_id, &model_state)
            .await;

        let effective_prompt = if fell_back {
            req.fallback_handoff_prompt
                .as_deref()
                .unwrap_or(&req.prompt)
        } else {
            &req.prompt
        };
        let prompt_fut = client.prompt_with_files(&acp_session_id, effective_prompt, &files);
        tokio::pin!(prompt_fut);

        let result = tokio::select! {
            _ = &mut cancel_rx => {
                let _ = client.cancel(&acp_session_id).await;
                self.withdraw_acp_permissions(&req.session_id).await;
                self.events.publish(SameEvent::SessionCancelled {
                    session_id: req.session_id.clone(),
                });
                self.events.publish(SameEvent::SessionStatusChanged {
                    session_id: req.session_id.clone(),
                    status: SessionStatus::Cancelled,
                });
                Ok(())
            }
            res = &mut prompt_fut => {
                let prompt_res = res?;
                self.events.publish(SameEvent::SessionCompleted {
                    session_id: req.session_id.clone(),
                    stop_reason: prompt_res.stop_reason,
                });
                self.events.publish(SameEvent::SessionStatusChanged {
                    session_id: req.session_id.clone(),
                    status: SessionStatus::Completed,
                });
                Ok(())
            }
        };
        if let Some(handle) = self.acp_sessions.lock().await.get(&req.session_id) {
            if let Ok(mut ts) = handle.last_used.lock() {
                *ts = Instant::now();
            }
        }
        result
    }

    /// Switches the ACP session to the requested model before a prompt. A
    /// failure is logged, not fatal: the prompt still runs on the agent's
    /// current model.
    async fn apply_acp_model(
        &self,
        req: &PromptRequest,
        client: &AcpClient,
        acp_session_id: &str,
        model_state: &SharedModelState,
    ) {
        let picked = req
            .model
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        // "Default": go back to the model a fresh session starts with, in
        // case this session was switched away from it earlier.
        let want = match picked {
            Some(m) => m,
            None => match self
                .acp_defaults
                .lock()
                .await
                .get(&req.provider.id)
                .and_then(|d| d.model.clone())
            {
                Some(d) => d,
                None => return,
            },
        };
        let want = want.as_str();
        let log = |line: String| {
            self.events.publish(SameEvent::AgentLog {
                session_id: Some(req.session_id.clone()),
                provider_id: req.provider.id.clone(),
                line,
            });
        };
        // A resumed session may not report its models; try the switch anyway.
        let state = model_state.lock().await.clone().unwrap_or_default();
        if state.current.as_deref() == Some(want) {
            return;
        }
        // Bounded so an agent that never answers can't block the prompt
        // (Stop is only watched once the prompt is sent).
        let res = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            client.set_model(acp_session_id, Some(&state), want),
        )
        .await
        .unwrap_or_else(|_| Err(anyhow::anyhow!("timed out")));
        match res {
            Ok(updated) => {
                let new_state = updated.unwrap_or_else(|| AcpModelState {
                    current: Some(want.to_string()),
                    ..state
                });
                self.events
                    .publish(models_event(&req.session_id, &req.provider.id, &new_state));
                *model_state.lock().await = Some(new_state);
            }
            Err(err) => log(format!("Failed to switch model to '{want}': {err:#}")),
        }
    }

    /// Sets the ACP session's reasoning effort before a prompt, when the
    /// agent offers one. Like the model, a failure is only logged.
    async fn apply_acp_effort(
        &self,
        req: &PromptRequest,
        client: &AcpClient,
        acp_session_id: &str,
        model_state: &SharedModelState,
    ) {
        let picked = req
            .effort
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let restoring = picked.is_none();
        // "Default": go back to the effort a fresh session starts with.
        let want = match picked {
            Some(e) => e,
            None => match self
                .acp_defaults
                .lock()
                .await
                .get(&req.provider.id)
                .and_then(|d| d.effort.clone())
            {
                Some(d) => d,
                None => return,
            },
        };
        let want = want.as_str();
        let log = |line: String| {
            self.events.publish(SameEvent::AgentLog {
                session_id: Some(req.session_id.clone()),
                provider_id: req.provider.id.clone(),
                line,
            });
        };
        let state = model_state.lock().await.clone().unwrap_or_default();
        if state.current_effort.as_deref() == Some(want) {
            return;
        }
        let Some(config_id) = state.effort_config_id.clone() else {
            if !restoring {
                log(format!(
                    "Effort '{want}' not applied: this agent offers no effort setting"
                ));
            }
            return;
        };
        // The default value (e.g. opencode's "default") isn't in the list.
        if !restoring && !state.efforts.iter().any(|e| e.id == want) {
            log(format!(
                "Effort '{want}' not applied: not offered for the current model"
            ));
            return;
        }
        let res = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            client.set_config_option(acp_session_id, &config_id, want),
        )
        .await
        .unwrap_or_else(|_| Err(anyhow::anyhow!("timed out")));
        match res {
            Ok(updated) => {
                let new_state = updated.unwrap_or_else(|| AcpModelState {
                    current_effort: Some(want.to_string()),
                    ..state
                });
                *model_state.lock().await = Some(new_state);
            }
            Err(err) => log(format!("Failed to set effort to '{want}': {err:#}")),
        }
    }

    async fn ensure_acp_session(
        &self,
        req: &PromptRequest,
        target: ExecTarget,
    ) -> Result<(Arc<AcpClient>, String, SharedModelState, bool)> {
        let wanted_skill_args = req
            .skill_delivery
            .as_ref()
            .map(|d| d.extra_args.clone())
            .unwrap_or_default();
        let wanted_skill_env = req
            .skill_delivery
            .as_ref()
            .map(|d| d.extra_env.clone())
            .unwrap_or_default();
        {
            let existing = {
                let guard = self.acp_sessions.lock().await;
                guard.get(&req.session_id).and_then(|handle| {
                    let ext_ok = req
                        .external_session_id
                        .as_deref()
                        .is_none_or(|ext| ext == handle.acp_session_id);
                    if handle.provider_id == req.provider.id
                        && handle.target == target
                        && handle.cwd == req.cwd
                        && handle.read_only == req.read_only
                        && ext_ok
                        && handle.skill_args == wanted_skill_args
                        && handle.skill_env == wanted_skill_env
                    {
                        Some((
                            handle.client.clone(),
                            handle.acp_session_id.clone(),
                            handle.model_state.clone(),
                            handle.modes.clone(),
                            handle.current_mode_id.clone(),
                        ))
                    } else {
                        None
                    }
                })
            };
            if let Some((client, acp_sid, model_state, modes, current_mode_id)) = existing {
                let mode_ok = match &modes {
                    Some(m) => match acp_mode_id(m, req.mode) {
                        Some(id) => match set_acp_mode_bounded(&client, &acp_sid, &id).await {
                            Ok(()) => {
                                *current_mode_id.lock().unwrap() = Some(id);
                                true
                            }
                            Err(_) => false,
                        },
                        None if req.mode == ApprovalMode::Plan => false,
                        None if req.mode == ApprovalMode::FullAccess => true,
                        None => {
                            let current = current_mode_id.lock().unwrap().clone();
                            let full_id = acp_mode_id(m, ApprovalMode::FullAccess);
                            let edit_id = acp_mode_id(m, ApprovalMode::AcceptEdits);
                            let stuck_in_full = current.is_some() && current == full_id;
                            let stuck_in_edits = req.mode == ApprovalMode::Ask
                                && current.is_some()
                                && current == edit_id;
                            !(stuck_in_full || stuck_in_edits)
                        }
                    },
                    None => req.mode != ApprovalMode::Plan,
                };
                if mode_ok {
                    return Ok((client, acp_sid, model_state, false));
                }
                // Could not confirm the requested mode on the pooled session;
                // drop it and start a fresh one.
                if let Some(old) = self.acp_sessions.lock().await.remove(&req.session_id) {
                    old.client.shutdown().await;
                }
            } else if let Some(old) = self.acp_sessions.lock().await.remove(&req.session_id) {
                // Provider, target, cwd, read-only, external session, or skill delivery config changed;
                // shut down the stale pooled process cleanly before spawning a new one.
                old.client.shutdown().await;
            }
        }

        let mut spawn_args = req.provider.args.clone();
        let mut spawn_env = req.provider.env.clone();
        if let Some(delivery) = &req.skill_delivery {
            spawn_args.extend(delivery.extra_args.iter().cloned());
            for (k, v) in &delivery.extra_env {
                spawn_env.insert(k.clone(), v.clone());
            }
        }

        let acp_cfg = AcpClientConfig {
            command: req.provider.command.clone(),
            args: spawn_args,
            env: spawn_env,
            cwd: req.cwd.clone(),
            // The approval mode is applied below, per prompt.
            auto_approve: false,
            target: target.clone(),
        };
        let (client, mut acp_rx) = if req.read_only {
            AcpClient::spawn_read_only(acp_cfg).await?
        } else {
            AcpClient::spawn_with_policy(
                acp_cfg,
                Arc::new(std::sync::atomic::AtomicBool::new(false)),
            )
            .await?
        };

        let client = Arc::new(client);
        let model_state: SharedModelState = Arc::new(Mutex::new(None));

        // Forward ACP events into SAME EventBus
        {
            let events = self.events.clone();
            let responders = self.permission_responders.clone();
            let acp_pending_permissions = self.acp_pending_permissions.clone();
            let acp_sessions = self.acp_sessions.clone();
            let this_client = client.clone();
            let model_state = model_state.clone();
            let workspace_cwd = req.cwd.clone();
            let agent_cwd = target.to_target_path(&req.cwd);
            let same_session_id = req.session_id.clone();
            let provider_id = req.provider.id.clone();
            let modes = self.modes.clone();

            tokio::spawn(async move {
                while let Some(ev) = acp_rx.recv().await {
                    match ev {
                        AcpEvent::SessionUpdate { update, .. } => match update {
                            SessionUpdate::AgentMessageChunk { text } => {
                                if !text.is_empty() {
                                    events.publish(SameEvent::MessageDelta {
                                        session_id: same_session_id.clone(),
                                        text,
                                    });
                                }
                            }
                            SessionUpdate::AgentThoughtChunk { text } => {
                                if !text.is_empty() {
                                    events.publish(SameEvent::ThinkingDelta {
                                        session_id: same_session_id.clone(),
                                        text,
                                    });
                                }
                            }
                            SessionUpdate::ToolCall {
                                tool_call_id,
                                title,
                                kind,
                                status,
                                raw_input,
                            } => {
                                events.publish(SameEvent::ToolCallStarted {
                                    session_id: same_session_id.clone(),
                                    tool_call: ToolCallRecord {
                                        id: tool_call_id,
                                        title,
                                        kind,
                                        status: parse_tool_status(&status),
                                        input: raw_input.map(|v| {
                                            serde_json::to_string_pretty(&v).unwrap_or_default()
                                        }),
                                        output: None,
                                    },
                                });
                            }
                            SessionUpdate::ToolCallUpdate {
                                tool_call_id,
                                title,
                                kind,
                                status,
                                raw_input,
                                raw_output,
                            } => {
                                events.publish(SameEvent::ToolCallUpdated {
                                    session_id: same_session_id.clone(),
                                    tool_call: ToolCallRecord {
                                        id: tool_call_id,
                                        title: title.unwrap_or_default(),
                                        kind,
                                        status: status
                                            .as_deref()
                                            .map(parse_tool_status)
                                            .unwrap_or(ToolCallStatus::InProgress),
                                        input: raw_input.map(|v| {
                                            serde_json::to_string_pretty(&v).unwrap_or_default()
                                        }),
                                        output: raw_output.map(|v| {
                                            serde_json::to_string_pretty(&v).unwrap_or_default()
                                        }),
                                    },
                                });
                            }
                            SessionUpdate::AvailableCommands { commands } => {
                                events.publish(SameEvent::SlashCommandsUpdated {
                                    session_id: same_session_id.clone(),
                                    commands: commands
                                        .into_iter()
                                        .map(|(name, description)| SlashCommandInfo {
                                            name,
                                            description,
                                        })
                                        .collect(),
                                });
                            }
                            SessionUpdate::ModelState(state) => {
                                events.publish(models_event(
                                    &same_session_id,
                                    &provider_id,
                                    &state,
                                ));
                                *model_state.lock().await = Some(state);
                            }
                            SessionUpdate::Other(_) => {}
                        },
                        AcpEvent::PermissionRequest {
                            request_id,
                            tool_call_id,
                            title,
                            description,
                            options,
                            tool_call,
                            responder,
                            ..
                        } => {
                            let is_edit =
                                tool_call.get("kind").and_then(Value::as_str) == Some("edit");
                            let auto = if is_edit
                                && acp_edit_within_workspace(&tool_call, &workspace_cwd, &agent_cwd)
                            {
                                AutoAllow::Edit
                            } else {
                                AutoAllow::InFullAccess
                            };
                            if auto.allows(mode_of(&modes, &same_session_id)) {
                                if let Some(opt) = options.iter().find(|o| o.kind == "allow_once") {
                                    responder.respond(PermissionDecision::Selected {
                                        option_id: opt.option_id.clone(),
                                    });
                                    continue;
                                }
                            }
                            let diff = diff::acp_tool_diff(&tool_call);
                            responders
                                .lock()
                                .await
                                .insert(request_id.clone(), responder);
                            let client_key = Arc::as_ptr(&this_client) as usize;
                            acp_pending_permissions
                                .lock()
                                .await
                                .entry(same_session_id.clone())
                                .or_default()
                                .push((client_key, request_id.clone()));
                            events.publish(SameEvent::SessionStatusChanged {
                                session_id: same_session_id.clone(),
                                status: SessionStatus::WaitingForApproval,
                            });
                            events.publish(SameEvent::ApprovalRequested {
                                request: PermissionRequest {
                                    request_id,
                                    session_id: same_session_id.clone(),
                                    tool_call_id,
                                    title,
                                    description,
                                    options: options
                                        .into_iter()
                                        .map(|o| PermissionOption {
                                            id: o.option_id,
                                            name: o.name,
                                            kind: o.kind,
                                        })
                                        .collect(),
                                    diff,
                                },
                            });
                        }
                        AcpEvent::StderrLine(line) => {
                            events.publish(SameEvent::AgentLog {
                                session_id: Some(same_session_id.clone()),
                                provider_id: provider_id.clone(),
                                line,
                            });
                        }
                        AcpEvent::ProcessExited(_) => {
                            let mut guard = acp_sessions.lock().await;
                            if guard
                                .get(&same_session_id)
                                .is_some_and(|h| Arc::ptr_eq(&h.client, &this_client))
                            {
                                guard.remove(&same_session_id);
                            }
                            drop(guard);
                            let client_key = Arc::as_ptr(&this_client) as usize;
                            let (own_ids, remaining) = {
                                let mut pending = acp_pending_permissions.lock().await;
                                if let Some(list) = pending.get_mut(&same_session_id) {
                                    let mut own = Vec::new();
                                    list.retain(|(ck, rid)| {
                                        if *ck == client_key {
                                            own.push(rid.clone());
                                            false
                                        } else {
                                            true
                                        }
                                    });
                                    let rem = list.len();
                                    if list.is_empty() {
                                        pending.remove(&same_session_id);
                                    }
                                    (own, rem)
                                } else {
                                    (Vec::new(), 0)
                                }
                            };
                            if !own_ids.is_empty() {
                                let mut resp_guard = responders.lock().await;
                                for rid in own_ids {
                                    if let Some(r) = resp_guard.remove(&rid) {
                                        r.respond(PermissionDecision::Cancelled);
                                    }
                                    events.publish(SameEvent::ApprovalWithdrawn {
                                        session_id: same_session_id.clone(),
                                        request_id: rid,
                                        remaining,
                                    });
                                }
                            }
                            break;
                        }
                    }
                }
            });
        }

        let mut fell_back = false;
        let loaded = match req.external_session_id.as_deref() {
            Some(ext_id) => match client.load_session(ext_id, &req.cwd).await {
                Ok((state, modes)) => Some((ext_id.to_string(), state, modes)),
                Err(err) => {
                    let reason = format!(
                        "ACP session/load failed for '{ext_id}': {err:#}; starting a new session"
                    );
                    self.events.publish(SameEvent::AgentLog {
                        session_id: Some(req.session_id.clone()),
                        provider_id: req.provider.id.clone(),
                        line: reason.clone(),
                    });
                    self.events.publish(SameEvent::NativeSessionFallback {
                        session_id: req.session_id.clone(),
                        provider_id: req.provider.id.clone(),
                        reason,
                    });
                    fell_back = true;
                    None
                }
            },
            None => None,
        };
        let (acp_session_id, initial_models, modes) = match loaded {
            Some(found) => found,
            None => {
                let res = client.new_session(&req.cwd).await?;
                if let Some(state) = &res.model_state {
                    self.record_acp_defaults(&req.provider.id, state).await;
                }
                (res.session_id, res.model_state, res.modes)
            }
        };
        // Start in the session's latest approval mode (re-read in case the
        // user changed it while the ACP process was starting).
        let mut confirmed_mode_id = modes
            .as_ref()
            .and_then(|m| m.get("currentModeId"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let effective_mode = mode_of(&self.modes, &req.session_id);
        if let Some(id) = modes.as_ref().and_then(|m| acp_mode_id(m, effective_mode)) {
            if confirmed_mode_id.as_deref() != Some(id.as_str()) {
                match set_acp_mode_bounded(&client, &acp_session_id, &id).await {
                    Ok(()) => confirmed_mode_id = Some(id),
                    Err(err) => {
                        if effective_mode != ApprovalMode::FullAccess {
                            client.shutdown().await;
                            return Err(err).with_context(|| {
                                format!("Failed to set ACP agent mode to '{id}'")
                            });
                        }
                    }
                }
            }
        } else if effective_mode == ApprovalMode::Plan {
            client.shutdown().await;
            anyhow::bail!(
                "Provider '{}' does not support Plan (read-only) mode",
                req.provider.name
            );
        } else if effective_mode != ApprovalMode::FullAccess {
            let full_id = modes
                .as_ref()
                .and_then(|m| acp_mode_id(m, ApprovalMode::FullAccess));
            if confirmed_mode_id.is_some() && confirmed_mode_id == full_id {
                client.shutdown().await;
                anyhow::bail!(
                    "Cannot downgrade ACP session from full access to '{}'",
                    effective_mode.as_str()
                );
            }
        }
        if let Some(state) = initial_models {
            self.events
                .publish(models_event(&req.session_id, &req.provider.id, &state));
            let mut guard = model_state.lock().await;
            // A config_option_update may already have arrived; keep it.
            if guard.is_none() {
                *guard = Some(state);
            }
        }

        self.events.publish(SameEvent::ExternalSessionBound {
            session_id: req.session_id.clone(),
            external_session_id: acp_session_id.clone(),
        });
        self.events.publish(SameEvent::ProviderSessionBound {
            session_id: req.session_id.clone(),
            provider_id: req.provider.id.clone(),
            external_session_id: acp_session_id.clone(),
        });

        let generation = self.acp_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let displaced = self.acp_sessions.lock().await.insert(
            req.session_id.clone(),
            AcpSessionHandle {
                generation,
                provider_id: req.provider.id.clone(),
                target,
                cwd: req.cwd.clone(),
                read_only: req.read_only,
                skill_args: wanted_skill_args,
                skill_env: wanted_skill_env,
                client: client.clone(),
                acp_session_id: acp_session_id.clone(),
                model_state: model_state.clone(),
                modes,
                current_mode_id: Arc::new(std::sync::Mutex::new(confirmed_mode_id)),
                last_used: Arc::new(std::sync::Mutex::new(Instant::now())),
            },
        );
        if let Some(old) = displaced {
            tokio::spawn(async move {
                old.client.shutdown().await;
            });
        }

        Ok((client, acp_session_id, model_state, fell_back))
    }

    async fn withdraw_acp_permissions(&self, session_id: &str) {
        let req_ids = self
            .acp_pending_permissions
            .lock()
            .await
            .remove(session_id)
            .unwrap_or_default();
        if req_ids.is_empty() {
            return;
        }
        let mut resp_guard = self.permission_responders.lock().await;
        for (_, rid) in req_ids {
            if let Some(r) = resp_guard.remove(&rid) {
                r.respond(PermissionDecision::Cancelled);
            }
            self.events.publish(SameEvent::ApprovalWithdrawn {
                session_id: session_id.to_string(),
                request_id: rid,
                remaining: 0,
            });
        }
    }

    pub async fn cancel_session(&self, session_id: &str) {
        if let Some(tx) = self.cancellers.lock().await.remove(session_id) {
            let _ = tx.send(());
        }
    }

    pub async fn respond_permission(&self, request_id: &str, option_id: Option<String>) {
        {
            let mut pending = self.acp_pending_permissions.lock().await;
            for ids in pending.values_mut() {
                ids.retain(|(_, id)| id != request_id);
            }
        }
        if let Some(responder) = self.permission_responders.lock().await.remove(request_id) {
            match option_id {
                Some(id) => responder.respond(PermissionDecision::Selected { option_id: id }),
                None => responder.respond(PermissionDecision::Cancelled),
            }
        }
    }

    pub async fn shutdown_all(&self) {
        {
            let mut guard = self.acp_sessions.lock().await;
            for (_, handle) in guard.drain() {
                handle.client.shutdown().await;
            }
        }
        self.claude_pool.shutdown_all().await;
        self.codex_pool.shutdown_all().await;
    }
}

/// Calls `session/set_mode` with a bounded wait so a stalled ACP agent cannot
/// hang session setup or mode switching indefinitely.
async fn set_acp_mode_bounded(client: &AcpClient, acp_sid: &str, mode_id: &str) -> Result<()> {
    tokio::time::timeout(
        std::time::Duration::from_secs(15),
        client.set_mode(acp_sid, mode_id),
    )
    .await
    .unwrap_or_else(|_| Err(anyhow::anyhow!("session/set_mode timed out")))
}

/// The id of the ACP agent mode matching an approval mode, from the agent's
/// `availableModes` (claude-agent-acp: default / acceptEdits / plan /
/// bypassPermissions; codex-acp: read-only / auto / full-access).
fn acp_mode_id(modes: &Value, mode: ApprovalMode) -> Option<String> {
    let available: Vec<&str> = modes
        .get("availableModes")?
        .as_array()?
        .iter()
        .filter_map(|m| m.get("id").and_then(Value::as_str))
        .collect();
    let candidates: &[&str] = match mode {
        ApprovalMode::Plan => &["plan", "read-only", "readonly"],
        ApprovalMode::Ask => &["default", "manual", "ask", "auto", "build"],
        ApprovalMode::AcceptEdits => &[
            "acceptEdits",
            "accept-edits",
            "auto-edit",
            "default",
            "manual",
            "auto",
            "build",
        ],
        ApprovalMode::FullAccess => &[
            "bypassPermissions",
            "full-access",
            "yolo",
            "dangerously-skip-permissions",
        ],
    };
    candidates.iter().find_map(|c| {
        available
            .iter()
            .find(|a| a.eq_ignore_ascii_case(c))
            .map(|a| a.to_string())
    })
}

fn parse_tool_status(s: &str) -> ToolCallStatus {
    match s {
        "completed" => ToolCallStatus::Completed,
        "failed" | "error" => ToolCallStatus::Failed,
        "denied" | "cancelled" => ToolCallStatus::Denied,
        "pending" => ToolCallStatus::Pending,
        _ => ToolCallStatus::InProgress,
    }
}

/// Returns `true` only when the ACP edit tool call names at least one target
/// path and every target path resolves (after following symlinks) inside the
/// session's workspace directory.
fn acp_edit_within_workspace(tool_call: &Value, workspace_cwd: &Path, agent_cwd: &str) -> bool {
    let mut paths: Vec<&str> = Vec::new();
    if let Some(locs) = tool_call.get("locations").and_then(Value::as_array) {
        for loc in locs {
            if let Some(p) = loc
                .get("path")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                paths.push(p);
            }
        }
    }
    if let Some(content) = tool_call.get("content").and_then(Value::as_array) {
        for item in content {
            if item.get("type").and_then(Value::as_str) == Some("diff") {
                if let Some(p) = item
                    .get("path")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    paths.push(p);
                }
            }
        }
    }
    if let Some(raw) = tool_call.get("rawInput") {
        for key in ["file_path", "path", "notebook_path"] {
            if let Some(p) = raw
                .get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                paths.push(p);
            }
        }
    }
    if paths.is_empty() {
        return false;
    }
    let Ok(canon_workspace) = std::fs::canonicalize(workspace_cwd) else {
        return false;
    };
    // If the agent runs inside a different filesystem namespace (e.g. WSL
    // where `agent_cwd` differs from `workspace_cwd`), we cannot reliably
    // resolve symlinks from the host; require explicit approval instead.
    if Path::new(agent_cwd) != workspace_cwd {
        return false;
    }
    paths.into_iter().all(|p| {
        let path = Path::new(p);
        // Reject parent traversal (`..`) so symlinks cannot be bypassed via
        // lexical normalization; require explicit approval instead.
        if path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return false;
        }
        let candidate = if path.is_absolute() {
            path.to_path_buf()
        } else {
            workspace_cwd.join(path)
        };
        canonicalize_with_missing_tail(&candidate)
            .is_some_and(|resolved| resolved.starts_with(&canon_workspace))
    })
}

/// Canonicalizes `path` by resolving the deepest existing ancestor via
/// `std::fs::canonicalize` and re-attaching the remaining non-existent tail.
/// Uses `symlink_metadata` so dangling symlinks fail closed instead of being
/// treated as ordinary missing filenames.
fn canonicalize_with_missing_tail(path: &Path) -> Option<PathBuf> {
    let mut existing = path;
    let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
    loop {
        match std::fs::symlink_metadata(existing) {
            Ok(_) => break,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                tail.push(existing.file_name()?);
                existing = existing.parent()?;
            }
            Err(_) => return None,
        }
    }
    let mut resolved = std::fs::canonicalize(existing).ok()?;
    for seg in tail.into_iter().rev() {
        resolved.push(seg);
    }
    Some(resolved)
}

/// Returns `true` if Codex has provider-configured API key env vars or CLI
/// config/profile flags that can supply credentials independently of
/// `codex login status`.
fn has_codex_runtime_auth_override(provider: &ProviderConfig) -> bool {
    let has_env_key = ["OPENAI_API_KEY", "CODEX_API_KEY"]
        .iter()
        .any(|k| provider.env.get(*k).is_some_and(|v| !v.trim().is_empty()));
    if has_env_key {
        return true;
    }
    provider.args.iter().any(|arg| {
        let a = arg.as_str();
        a == "-c"
            || a == "--config"
            || a.starts_with("--config=")
            || a == "-p"
            || a == "--profile"
            || a.starts_with("--profile=")
    })
}

/// Returns `true` if `args` includes Claude CLI flags that alter credential or
/// settings resolution at prompt time (`--settings`, `--setting-sources`,
/// `--bare`) and cannot be evaluated by `claude auth status`.
fn has_claude_credential_arg_override(args: &[String]) -> bool {
    args.iter().any(|arg| {
        let a = arg.as_str();
        a == "--settings"
            || a.starts_with("--settings=")
            || a == "--setting-sources"
            || a.starts_with("--setting-sources=")
            || a == "--bare"
    })
}

/// Returns `true` when `claude auth status` indicates a first-party `claude.ai`
/// OAuth login with no API key override (meaning an active Claude subscription
/// or extra usage is required to run prompts).
fn requires_claude_subscription(val: &Value) -> bool {
    let is_claude_ai = val
        .get("authMethod")
        .and_then(Value::as_str)
        .is_some_and(|m| m.eq_ignore_ascii_case("claude.ai"));
    if !is_claude_ai {
        return false;
    }
    let is_first_party = val
        .get("apiProvider")
        .and_then(Value::as_str)
        .is_none_or(|p| p.eq_ignore_ascii_case("firstparty"));
    if !is_first_party {
        return false;
    }
    let has_api_key = val
        .get("apiKeySource")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.trim().is_empty() && !s.eq_ignore_ascii_case("none"));
    !has_api_key
}

fn is_non_paid_plan_value(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::String(s) => {
            let t = s.trim();
            t.is_empty() || t.eq_ignore_ascii_case("none") || t.eq_ignore_ascii_case("free")
        }
        _ => false,
    }
}

fn parse_claude_auth_status(success: bool, stdout: &str) -> ProviderAuthStatus {
    if let Ok(val) = serde_json::from_str::<Value>(stdout.trim()) {
        if let Some(logged_in) = val.get("loggedIn").and_then(Value::as_bool) {
            if !logged_in {
                return ProviderAuthStatus::NotLoggedIn;
            }
            if requires_claude_subscription(&val) {
                let sub = val.get("subscriptionType");
                let tier = val.get("planTier");
                let sub_explicitly_unpaid = sub.is_some_and(is_non_paid_plan_value);
                let tier_explicitly_unpaid = tier.is_some_and(is_non_paid_plan_value);
                let sub_paid = sub.is_some_and(|v| !is_non_paid_plan_value(v));
                let tier_paid = tier.is_some_and(|v| !is_non_paid_plan_value(v));
                if (sub_explicitly_unpaid && !tier_paid) || (tier_explicitly_unpaid && !sub_paid) {
                    return ProviderAuthStatus::NoSubscription;
                }
            }
            return ProviderAuthStatus::LoggedIn;
        }
    }
    if success {
        ProviderAuthStatus::LoggedIn
    } else {
        ProviderAuthStatus::Unknown
    }
}

/// Returns candidate `.claude.json` paths (in the target's path format) for a
/// Claude `configDirectory`.
///
/// When `CLAUDE_CONFIG_DIR` is set, Claude stores `.claude.json` inside that
/// directory; otherwise `configDirectory` is `~/.claude` and the state file is
/// the sibling `~/.claude.json`.
fn claude_state_file_candidates(config_dir: &str) -> Vec<String> {
    let trimmed = config_dir.trim().trim_end_matches(['/', '\\']);
    if trimmed.is_empty() {
        return Vec::new();
    }
    let sep = if trimmed.contains('\\') && !trimmed.starts_with('/') {
        '\\'
    } else {
        '/'
    };
    let mut out = vec![format!("{trimmed}{sep}.claude.json")];
    if let Some(pos) = trimmed.rfind(['/', '\\']) {
        let last = &trimmed[pos + 1..];
        if last.eq_ignore_ascii_case(".claude") {
            let parent = &trimmed[..pos];
            if parent.is_empty() {
                out.push(format!("{sep}.claude.json"));
            } else {
                out.push(format!("{parent}{sep}.claude.json"));
            }
        }
    }
    out
}

fn refine_claude_auth_with_state(
    status_val: &Value,
    state_val: &Value,
    base_status: ProviderAuthStatus,
    now: chrono::DateTime<chrono::Utc>,
) -> ProviderAuthStatus {
    let Some(account) = state_val.get("oauthAccount").filter(|v| v.is_object()) else {
        return base_status;
    };
    // Guard against mismatched cached account state.
    if let (Some(status_org), Some(state_org)) = (
        status_val
            .get("orgId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty()),
        account
            .get("organizationUuid")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty()),
    ) {
        if status_org != state_org {
            return base_status;
        }
    }
    if let (Some(status_email), Some(state_email)) = (
        status_val
            .get("email")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty()),
        account
            .get("emailAddress")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty()),
    ) {
        if !status_email.eq_ignore_ascii_case(state_email) {
            return base_status;
        }
    }
    // Extra usage (pay-as-you-go credits) or an active trial keeps the account usable.
    if account.get("hasExtraUsageEnabled").and_then(Value::as_bool) == Some(true) {
        return ProviderAuthStatus::LoggedIn;
    }
    if let Some(trial_end) = account
        .get("claudeCodeTrialEndsAt")
        .and_then(Value::as_str)
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
    {
        if trial_end.with_timezone(&chrono::Utc) > now {
            return ProviderAuthStatus::LoggedIn;
        }
    }
    let billing_none = account
        .get("billingType")
        .and_then(Value::as_str)
        .is_some_and(|b| b.trim().is_empty() || b.eq_ignore_ascii_case("none"));
    let no_available_sub = account
        .get("hasAvailableSubscription")
        .and_then(Value::as_bool)
        == Some(false);
    if billing_none || no_available_sub {
        return ProviderAuthStatus::NoSubscription;
    }
    base_status
}

fn parse_codex_auth_status(success: bool, stdout: &str, stderr: &str) -> ProviderAuthStatus {
    let combined = format!("{stdout}\n{stderr}").to_ascii_lowercase();
    if combined.contains("not logged in") {
        return ProviderAuthStatus::NotLoggedIn;
    }
    if success && combined.contains("logged in") {
        return ProviderAuthStatus::LoggedIn;
    }
    ProviderAuthStatus::Unknown
}

fn parse_opencode_auth_status(success: bool, stdout: &str, stderr: &str) -> ProviderAuthStatus {
    if !success {
        return ProviderAuthStatus::Unknown;
    }
    let combined = format!("{stdout}\n{stderr}").to_ascii_lowercase();
    if combined.contains("no credentials") || combined.contains("0 credentials") {
        // OpenCode can still run free or local models without stored credentials.
        return ProviderAuthStatus::Unknown;
    }
    // Check if there is at least one non-empty credential line (skipping box/header chrome).
    let has_cred = stdout.lines().any(|line| {
        let trimmed = line.trim();
        !trimmed.is_empty()
            && !trimmed.starts_with('┌')
            && !trimmed.starts_with('│')
            && !trimmed.starts_with('└')
            && !trimmed.starts_with('─')
            && !trimmed.starts_with('◆')
            && !trimmed.starts_with('◇')
            && !trimmed.starts_with("Credentials")
    });
    if has_cred {
        ProviderAuthStatus::LoggedIn
    } else {
        ProviderAuthStatus::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use same_core::config::default_providers;

    #[tokio::test]
    async fn test_live_opencode_acp_handshake_if_installed() {
        let events = EventBus::new(64);
        let mgr = AgentManager::new(events, tokio::runtime::Handle::current());
        let providers = default_providers();
        let opencode = providers.into_iter().find(|p| p.id == "opencode").unwrap();
        let cwd = std::env::current_dir().unwrap();
        let chk = mgr
            .check_provider(&opencode, ExecPreference::Auto, &cwd)
            .await;
        if !chk.available {
            return;
        }
        let info = mgr
            .test_acp_connection(&opencode, ExecPreference::Auto, cwd)
            .await
            .expect("opencode acp handshake should succeed");
        assert!(!info.is_empty());
    }

    /// Picking "Default" after switching an ACP session to another model
    /// and effort switches it back (live opencode; skipped if missing).
    #[tokio::test]
    async fn default_restores_acp_model_and_effort() {
        let events = EventBus::new(256);
        let mgr = AgentManager::new(events, tokio::runtime::Handle::current());
        let opencode = default_providers()
            .into_iter()
            .find(|p| p.id == "opencode")
            .unwrap();
        let cwd = std::env::temp_dir();
        if !mgr
            .check_provider(&opencode, ExecPreference::Auto, &cwd)
            .await
            .available
        {
            return;
        }
        let target = resolve_target(ExecPreference::Auto, &cwd).await;
        let (client, _rx) = AcpClient::spawn(AcpClientConfig {
            command: opencode.command.clone(),
            args: opencode.args.clone(),
            env: opencode.env.clone(),
            cwd: cwd.clone(),
            auto_approve: false,
            target,
        })
        .await
        .unwrap();
        let res = client.new_session(&cwd).await.unwrap();
        let initial = res.model_state.clone().expect("opencode reports models");
        mgr.record_acp_defaults(&opencode.id, &initial).await;
        let other_model = initial
            .models
            .iter()
            .find(|m| Some(&m.id) != initial.current.as_ref())
            .expect("a second model")
            .id
            .clone();
        let other_effort = initial
            .efforts
            .iter()
            .find(|e| Some(&e.id) != initial.current_effort.as_ref())
            .map(|e| e.id.clone());
        let state: SharedModelState = Arc::new(Mutex::new(Some(initial.clone())));
        let mut req = PromptRequest {
            session_id: "t".into(),
            external_session_id: None,
            provider: opencode.clone(),
            model: None,
            effort: other_effort.clone(),
            cwd: cwd.clone(),
            prompt: String::new(),
            attachments: Vec::new(),
            exec: ExecPreference::Auto,
            mode: ApprovalMode::Ask,
            read_only: false,
            fallback_handoff_prompt: None,
            skill_delivery: None,
            skill_sync: None,
        };
        let apply = |req: PromptRequest| {
            let (mgr, client, sid, state) = (&mgr, &client, res.session_id.clone(), state.clone());
            async move {
                mgr.apply_acp_model(&req, client, &sid, &state).await;
                mgr.apply_acp_effort(&req, client, &sid, &state).await;
                state.lock().await.clone().unwrap()
            }
        };

        // Effort: pick another level on the default model, then "Default".
        if let Some(e) = &other_effort {
            let st = apply(req.clone()).await;
            assert_eq!(st.current_effort.as_deref(), Some(e.as_str()));
            req.effort = None;
            let st = apply(req.clone()).await;
            assert_eq!(st.current_effort, initial.current_effort);
        }

        // Model: switch away, then "Default".
        req.model = Some(other_model.clone());
        let st = apply(req.clone()).await;
        assert_eq!(st.current.as_deref(), Some(other_model.as_str()));
        req.model = None;
        let st = apply(req.clone()).await;
        assert_eq!(st.current, initial.current);
        client.shutdown().await;
    }

    /// End-to-end through WSL from a Windows process. Run on Windows with
    /// `SAME_WSL_TEST_DIR` (a `\\wsl.localhost\...` folder) and
    /// `SAME_WSL_FAKE_ACP` (Linux path of a fake ACP agent script), `--ignored`.
    #[cfg(windows)]
    #[tokio::test]
    #[ignore]
    async fn prompts_agents_inside_wsl() {
        use same_core::{ProviderKind, SameEvent};
        use std::collections::BTreeMap;

        let dir = PathBuf::from(std::env::var("SAME_WSL_TEST_DIR").unwrap());
        let fake = std::env::var("SAME_WSL_FAKE_ACP").unwrap();
        let events = EventBus::new(4096);
        let mgr = AgentManager::new(events.clone(), tokio::runtime::Handle::current());

        let run = |provider: ProviderConfig, sid: &str| {
            let mut rx = events.subscribe();
            mgr.start_prompt(PromptRequest {
                session_id: sid.to_string(),
                external_session_id: None,
                provider,
                model: None,
                effort: None,
                cwd: dir.clone(),
                prompt: "Reply with just the word OK.".to_string(),
                attachments: Vec::new(),
                exec: ExecPreference::Auto,
                mode: ApprovalMode::FullAccess,
                read_only: false,
                fallback_handoff_prompt: None,
                skill_delivery: None,
                skill_sync: None,
            });
            async move {
                let mut text = String::new();
                loop {
                    match tokio::time::timeout(std::time::Duration::from_secs(120), rx.recv())
                        .await
                        .expect("timed out")
                        .unwrap()
                    {
                        SameEvent::MessageDelta { text: t, .. } => text.push_str(&t),
                        SameEvent::SessionCompleted { .. } => return Ok(text),
                        SameEvent::SessionFailed { error, .. } => return Err(error),
                        _ => {}
                    }
                }
            }
        };

        let fake_provider = ProviderConfig {
            id: "fake".into(),
            name: "Fake".into(),
            kind: ProviderKind::Acp,
            enabled: true,
            command: "python3".into(),
            args: vec![fake],
            env: BTreeMap::new(),
            default_model: None,
            models: vec![],
            description: String::new(),
        };
        let text = run(fake_provider, "s-fake").await.expect("fake ACP failed");
        eprintln!("fake ACP replied {} chars", text.len());
        assert!(text.contains("line 60"));

        let claude = same_core::config::default_providers()
            .into_iter()
            .find(|p| p.id == "claude")
            .unwrap();
        let text = run(claude, "s-claude").await.expect("claude failed");
        eprintln!("claude replied: {text}");
        assert!(text.to_uppercase().contains("OK"));
    }

    #[test]
    fn picks_setup_commands_per_provider() {
        let providers = default_providers();
        let get = |id: &str| providers.iter().find(|p| p.id == id).unwrap().clone();
        assert_eq!(
            AgentManager::login_script(&get("claude")).unix,
            "claude auth login"
        );
        assert_eq!(
            AgentManager::login_script(&get("codex")).unix,
            "codex login"
        );
        assert_eq!(
            AgentManager::login_script(&get("opencode")).unix,
            "opencode auth login"
        );
        assert_eq!(
            AgentManager::login_script(&get("antigravity")).unix,
            "agy acp --login"
        );
        assert!(AgentManager::install_script(&get("antigravity")).is_some());
        assert!(AgentManager::install_script(&get("claude")).is_none());
        assert!(AgentManager::supports_in_app_login(&get("antigravity")));
        assert!(!AgentManager::supports_in_app_login(&get("claude")));

        // Multi-byte UTF-8 character split across two reads is preserved.
        let bytes = "認証OK".as_bytes();
        let mut pending = bytes[..4].to_vec(); // "認" (3 bytes) + 1st byte of "証"
        assert_eq!(drain_utf8_chunks(&mut pending), "認");
        assert_eq!(pending.len(), 1);
        pending.extend_from_slice(&bytes[4..]);
        assert_eq!(drain_utf8_chunks(&mut pending), "証OK");
        assert!(pending.is_empty());

        let mut windows_agy = get("antigravity");
        windows_agy.id = "agy-win".into();
        windows_agy.command = r"C:\Users\me\AppData\Local\agy\bin\agy.exe".into();
        assert!(AgentManager::install_script(&windows_agy).is_some());
        assert_eq!(
            AgentManager::login_script(&windows_agy).windows,
            r"& 'C:\Users\me\AppData\Local\agy\bin\agy.exe' 'acp' '--login'"
        );
    }

    #[test]
    fn acp_modes_match_by_id() {
        let claude_acp = serde_json::json!({"currentModeId": "default", "availableModes": [
            {"id": "default", "name": "Default"}, {"id": "acceptEdits", "name": "Accept Edits"},
            {"id": "plan", "name": "Plan"}, {"id": "bypassPermissions", "name": "Bypass"}
        ]});
        assert_eq!(
            acp_mode_id(&claude_acp, ApprovalMode::Plan).as_deref(),
            Some("plan")
        );
        assert_eq!(
            acp_mode_id(&claude_acp, ApprovalMode::Ask).as_deref(),
            Some("default")
        );
        assert_eq!(
            acp_mode_id(&claude_acp, ApprovalMode::AcceptEdits).as_deref(),
            Some("acceptEdits")
        );
        assert_eq!(
            acp_mode_id(&claude_acp, ApprovalMode::FullAccess).as_deref(),
            Some("bypassPermissions")
        );
        let codex_acp = serde_json::json!({"availableModes": [
            {"id": "read-only"}, {"id": "auto"}, {"id": "full-access"}
        ]});
        assert_eq!(
            acp_mode_id(&codex_acp, ApprovalMode::Ask).as_deref(),
            Some("auto")
        );
        assert_eq!(
            acp_mode_id(&codex_acp, ApprovalMode::AcceptEdits).as_deref(),
            Some("auto")
        );
        assert_eq!(
            acp_mode_id(&codex_acp, ApprovalMode::FullAccess).as_deref(),
            Some("full-access")
        );
        assert_eq!(
            acp_mode_id(&codex_acp, ApprovalMode::Plan).as_deref(),
            Some("read-only")
        );
        assert_eq!(
            acp_mode_id(&serde_json::json!({}), ApprovalMode::Plan),
            None
        );
    }

    #[test]
    fn parse_auth_status_helpers() {
        assert_eq!(
            parse_claude_auth_status(true, r#"{"loggedIn": true, "authMethod": "claude.ai"}"#),
            ProviderAuthStatus::LoggedIn
        );
        assert_eq!(
            parse_claude_auth_status(false, r#"{"loggedIn": false}"#),
            ProviderAuthStatus::NotLoggedIn
        );
        // Logged into claude.ai firstParty with no subscription (null / free / none)
        assert_eq!(
            parse_claude_auth_status(
                true,
                r#"{"loggedIn": true, "authMethod": "claude.ai", "apiProvider": "firstParty", "subscriptionType": null}"#
            ),
            ProviderAuthStatus::NoSubscription
        );
        assert_eq!(
            parse_claude_auth_status(
                true,
                r#"{"loggedIn": true, "authMethod": "claude.ai", "apiProvider": "firstParty", "subscriptionType": "free", "planTier": "free"}"#
            ),
            ProviderAuthStatus::NoSubscription
        );
        // API key overrides claude.ai login: usable without a subscription
        assert_eq!(
            parse_claude_auth_status(
                true,
                r#"{"loggedIn": true, "authMethod": "claude.ai", "apiProvider": "firstParty", "apiKeySource": "ANTHROPIC_API_KEY", "subscriptionType": null}"#
            ),
            ProviderAuthStatus::LoggedIn
        );
        // Third-party provider (Bedrock/Vertex) does not need a claude.ai subscription
        assert_eq!(
            parse_claude_auth_status(
                true,
                r#"{"loggedIn": true, "authMethod": "claude.ai", "apiProvider": "bedrock", "subscriptionType": null}"#
            ),
            ProviderAuthStatus::LoggedIn
        );
        // Lapsed Pro subscription: auth status has stale "pro", .claude.json has billingType "none"
        let now = chrono::DateTime::parse_from_rfc3339("2026-11-27T09:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let stale_status: Value = serde_json::from_str(
            r#"{"loggedIn": true, "authMethod": "claude.ai", "apiProvider": "firstParty", "email": "u@example.com", "orgId": "org-1", "subscriptionType": "pro", "planTier": "pro"}"#,
        )
        .unwrap();
        let lapsed_state: Value = serde_json::from_str(
            r#"{"oauthAccount": {"emailAddress": "u@example.com", "organizationUuid": "org-1", "billingType": "none", "hasExtraUsageEnabled": false, "claudeCodeTrialEndsAt": null}}"#,
        )
        .unwrap();
        assert_eq!(
            refine_claude_auth_with_state(
                &stale_status,
                &lapsed_state,
                ProviderAuthStatus::LoggedIn,
                now
            ),
            ProviderAuthStatus::NoSubscription
        );
        // Active Stripe subscription remains LoggedIn
        let active_state: Value = serde_json::from_str(
            r#"{"oauthAccount": {"emailAddress": "u@example.com", "organizationUuid": "org-1", "billingType": "stripe_subscription", "hasExtraUsageEnabled": false}}"#,
        )
        .unwrap();
        assert_eq!(
            refine_claude_auth_with_state(
                &stale_status,
                &active_state,
                ProviderAuthStatus::LoggedIn,
                now
            ),
            ProviderAuthStatus::LoggedIn
        );
        // Extra usage enabled or active trial keeps account usable even when base status is NoSubscription
        let free_status: Value = serde_json::from_str(
            r#"{"loggedIn": true, "authMethod": "claude.ai", "apiProvider": "firstParty", "email": "u@example.com", "orgId": "org-1", "subscriptionType": null}"#,
        )
        .unwrap();
        let extra_usage_state: Value = serde_json::from_str(
            r#"{"oauthAccount": {"emailAddress": "u@example.com", "organizationUuid": "org-1", "billingType": "none", "hasExtraUsageEnabled": true}}"#,
        )
        .unwrap();
        assert_eq!(
            refine_claude_auth_with_state(
                &free_status,
                &extra_usage_state,
                ProviderAuthStatus::NoSubscription,
                now
            ),
            ProviderAuthStatus::LoggedIn
        );
        let trial_state: Value = serde_json::from_str(
            r#"{"oauthAccount": {"emailAddress": "u@example.com", "organizationUuid": "org-1", "billingType": "none", "hasExtraUsageEnabled": false, "claudeCodeTrialEndsAt": "2026-12-01T00:00:00Z"}}"#,
        )
        .unwrap();
        assert_eq!(
            refine_claude_auth_with_state(
                &free_status,
                &trial_state,
                ProviderAuthStatus::NoSubscription,
                now
            ),
            ProviderAuthStatus::LoggedIn
        );
        // Mismatched orgId in .claude.json preserves base status
        let other_org_state: Value = serde_json::from_str(
            r#"{"oauthAccount": {"emailAddress": "u@example.com", "organizationUuid": "org-other", "billingType": "none", "hasExtraUsageEnabled": false}}"#,
        )
        .unwrap();
        assert_eq!(
            refine_claude_auth_with_state(
                &stale_status,
                &other_org_state,
                ProviderAuthStatus::LoggedIn,
                now
            ),
            ProviderAuthStatus::LoggedIn
        );
        assert_eq!(
            refine_claude_auth_with_state(
                &free_status,
                &other_org_state,
                ProviderAuthStatus::NoSubscription,
                now
            ),
            ProviderAuthStatus::NoSubscription
        );
        // State file candidate path resolution (Linux default, custom CLAUDE_CONFIG_DIR, Windows)
        assert_eq!(
            claude_state_file_candidates("/home/sota/.claude"),
            vec![
                "/home/sota/.claude/.claude.json".to_string(),
                "/home/sota/.claude.json".to_string(),
            ]
        );
        assert_eq!(
            claude_state_file_candidates("/tmp/custom-claude"),
            vec!["/tmp/custom-claude/.claude.json".to_string()]
        );
        assert_eq!(
            claude_state_file_candidates(r"C:\Users\kamur\.claude"),
            vec![
                r"C:\Users\kamur\.claude\.claude.json".to_string(),
                r"C:\Users\kamur\.claude.json".to_string(),
            ]
        );
        // Runtime subscription error detection
        assert!(is_claude_subscription_error(
            "Claude Code can't use your Claude subscription, usually because a payment is past due"
        ));
        assert!(is_claude_subscription_error(
            "403 oauth_not_allowed_for_organization"
        ));
        assert!(!is_claude_subscription_error("network connection reset"));
        // Credential-overriding Claude args skip auth-status rejection
        assert!(has_claude_credential_arg_override(&[
            "--settings".to_string(),
            "/tmp/s.json".to_string()
        ]));
        assert!(has_claude_credential_arg_override(&[
            "--settings=/tmp/s.json".to_string()
        ]));
        assert!(has_claude_credential_arg_override(&["--bare".to_string()]));
        assert!(!has_claude_credential_arg_override(&[
            "--model".to_string(),
            "sonnet".to_string()
        ]));
        // Codex runtime env/config overrides prevent false NotLoggedIn hiding
        let mut codex_cfg = default_providers()
            .into_iter()
            .find(|p| p.id == "codex")
            .unwrap();
        assert!(!has_codex_runtime_auth_override(&codex_cfg));
        codex_cfg
            .env
            .insert("OPENAI_API_KEY".to_string(), "sk-test".to_string());
        assert!(has_codex_runtime_auth_override(&codex_cfg));
        codex_cfg.env.clear();
        codex_cfg.args = vec!["--profile=work".to_string()];
        assert!(has_codex_runtime_auth_override(&codex_cfg));
        assert_eq!(
            parse_codex_auth_status(true, "Logged in using ChatGPT\n", ""),
            ProviderAuthStatus::LoggedIn
        );
        assert_eq!(
            parse_codex_auth_status(false, "", "Not logged in\n"),
            ProviderAuthStatus::NotLoggedIn
        );
        // Inconclusive Codex failure does not falsely mark NotLoggedIn
        assert_eq!(
            parse_codex_auth_status(false, "", "error: unrecognized subcommand 'status'\n"),
            ProviderAuthStatus::Unknown
        );
        assert_eq!(
            parse_opencode_auth_status(
                true,
                "Google  GOOGLE_API_KEY              environment\n",
                ""
            ),
            ProviderAuthStatus::LoggedIn
        );
        // OpenCode can still run free/local models with no stored credentials
        assert_eq!(
            parse_opencode_auth_status(true, "No credentials found\n", ""),
            ProviderAuthStatus::Unknown
        );
        assert_eq!(
            parse_opencode_auth_status(true, "   \n", ""),
            ProviderAuthStatus::Unknown
        );
    }

    #[tokio::test]
    async fn parallel_concurrency_limit_and_duplicate_run_guard() {
        let events = EventBus::new(256);
        let mgr = AgentManager::new(events.clone(), tokio::runtime::Handle::current());
        assert_eq!(mgr.max_concurrent_sessions(), None);
        mgr.set_max_concurrent_sessions(Some(2));
        assert_eq!(mgr.max_concurrent_sessions(), Some(2));

        // Use a command that sleeps so the turns stay active until cancelled.
        let slow_provider = ProviderConfig {
            id: "slow-test".into(),
            name: "Slow Test".into(),
            kind: ProviderKind::Acp,
            enabled: true,
            command: "sh".into(),
            args: vec!["-c".into(), "sleep 30".into()],
            env: std::collections::BTreeMap::new(),
            default_model: None,
            models: vec![],
            description: String::new(),
        };
        let make_req = |sid: &str| PromptRequest {
            session_id: sid.to_string(),
            external_session_id: None,
            provider: slow_provider.clone(),
            model: None,
            effort: None,
            cwd: std::env::temp_dir(),
            prompt: "hello".into(),
            attachments: vec![],
            exec: ExecPreference::Auto,
            mode: ApprovalMode::Ask,
            read_only: false,
            fallback_handoff_prompt: None,
            skill_delivery: None,
            skill_sync: None,
        };

        // Start session 1 -> succeeds.
        mgr.try_start_prompt(make_req("s1")).unwrap();
        assert!(mgr.is_session_running("s1"));
        assert_eq!(mgr.active_session_count(), 1);

        // Duplicate start on session 1 -> rejected.
        let dup = mgr.try_start_prompt(make_req("s1"));
        assert!(dup.is_err());

        // Start session 2 in parallel -> succeeds (2/2).
        mgr.try_start_prompt(make_req("s2")).unwrap();
        assert_eq!(mgr.active_session_count(), 2);

        // Start session 3 -> rejected because concurrency limit is 2.
        let over = mgr.try_start_prompt(make_req("s3"));
        assert!(over.is_err());

        // Cancel only s1 and s2 cleanly.
        mgr.cancel_session("s1").await;
        mgr.cancel_session("s2").await;
        mgr.shutdown_all().await;
    }
}
