use crate::adapters::{run_claude_cli, run_codex_cli};
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
    EventBus, ModelInfo, PermissionOption, PermissionRequest, ProviderCapabilities, ProviderConfig,
    ProviderKind, SameEvent, SessionStatus, SlashCommandInfo, ToolCallRecord, ToolCallStatus,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use tokio::sync::{oneshot, Mutex};

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
    pub auto_approve: bool,
    /// Where the project's agents should run (see [`ExecPreference`]).
    pub exec: ExecPreference,
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

/// `C:\x\agy.exe` or `/usr/bin/agy` -> `agy`.
fn command_name(command: &str) -> String {
    let base = command.rsplit(['/', '\\']).next().unwrap_or(command);
    base.strip_suffix(".exe")
        .or_else(|| base.strip_suffix(".cmd"))
        .unwrap_or(base)
        .to_ascii_lowercase()
}

#[derive(Debug, Clone)]
pub struct ProviderCheckResult {
    pub provider_id: String,
    pub available: bool,
    pub detail: String,
    pub capabilities: ProviderCapabilities,
    /// Where the check ran, e.g. "Windows" or "WSL: Ubuntu".
    pub target: String,
}

struct AcpSessionHandle {
    provider_id: String,
    target: ExecTarget,
    client: Arc<AcpClient>,
    acp_session_id: String,
    /// The models the agent offers and the one in use; also updated by the
    /// agent's `config_option_update` notifications.
    model_state: SharedModelState,
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
    cancellers: Arc<Mutex<HashMap<String, oneshot::Sender<()>>>>,
    permission_responders: Arc<Mutex<HashMap<String, PermissionResponder>>>,
    /// Per ACP provider: the model and effort a fresh session starts with,
    /// restored when a session goes back to "Default".
    acp_defaults: Arc<Mutex<HashMap<String, AcpDefaults>>>,
}

#[derive(Debug, Clone, Default)]
struct AcpDefaults {
    model: Option<String>,
    /// As the agent reports it; may be "default".
    effort: Option<String>,
}

impl AgentManager {
    pub fn new(events: EventBus, runtime: tokio::runtime::Handle) -> Self {
        Self {
            events,
            runtime,
            acp_sessions: Arc::new(Mutex::new(HashMap::new())),
            cancellers: Arc::new(Mutex::new(HashMap::new())),
            permission_responders: Arc::new(Mutex::new(HashMap::new())),
            acp_defaults: Arc::new(Mutex::new(HashMap::new())),
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
            },
            ProviderKind::ClaudeCli => ProviderCapabilities {
                supports_acp: false,
                supports_streaming: true,
                supports_thinking: true,
                supports_tool_calls: true,
                supports_permissions: false,
                supports_cancel: true,
                supports_session_resume: true,
                supports_model_select: true,
                supports_slash_commands: false,
            },
            ProviderKind::CodexCli => ProviderCapabilities {
                supports_acp: false,
                supports_streaming: true,
                supports_thinking: true,
                supports_tool_calls: true,
                supports_permissions: false,
                supports_cancel: true,
                supports_session_resume: true,
                supports_model_select: true,
                supports_slash_commands: false,
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
            .stderr(Stdio::piped());
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
                ProviderCheckResult {
                    provider_id: provider.id.clone(),
                    available,
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
                detail: format!("Not found ({}): {e}", provider.command),
                capabilities: caps,
                target: target.label(),
            },
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
        let mut cmd = build_console_command(&target, cwd, &script, cfg!(windows)).await;
        for (k, v) in &provider.env {
            cmd.env(k, v);
        }

        let message = if cfg!(windows) {
            let status = cmd
                .status()
                .await
                .with_context(|| format!("Failed to open a terminal for {}", target.label()))?;
            format!("Terminal closed (exit {})", status.code().unwrap_or(-1))
        } else {
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

    pub fn start_prompt(&self, req: PromptRequest) {
        let manager = self.clone();
        self.runtime.spawn(async move {
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

            let (cancel_tx, cancel_rx) = oneshot::channel();
            manager
                .cancellers
                .lock()
                .await
                .insert(sid.clone(), cancel_tx);

            let target = resolve_target(req.exec, &req.cwd).await;
            let res = match req.provider.kind {
                ProviderKind::Acp => manager.run_acp_prompt(req, target, cancel_rx).await,
                ProviderKind::ClaudeCli => {
                    run_claude_cli(
                        &req.provider,
                        &req.session_id,
                        req.external_session_id.as_deref(),
                        req.model.as_deref(),
                        req.effort.as_deref(),
                        &req.cwd,
                        &target,
                        &req.prompt,
                        req.auto_approve,
                        manager.events.clone(),
                        cancel_rx,
                    )
                    .await
                }
                ProviderKind::CodexCli => {
                    run_codex_cli(
                        &req.provider,
                        &req.session_id,
                        req.external_session_id.as_deref(),
                        req.model.as_deref(),
                        req.effort.as_deref(),
                        &req.cwd,
                        &target,
                        &req.prompt,
                        req.auto_approve,
                        manager.events.clone(),
                        cancel_rx,
                    )
                    .await
                }
            };

            manager.cancellers.lock().await.remove(&sid);

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
    }

    async fn run_acp_prompt(
        &self,
        req: PromptRequest,
        target: ExecTarget,
        mut cancel_rx: oneshot::Receiver<()>,
    ) -> Result<()> {
        let (client, acp_session_id, model_state) = self.ensure_acp_session(&req, target).await?;
        self.apply_acp_model(&req, &client, &acp_session_id, &model_state)
            .await;
        self.apply_acp_effort(&req, &client, &acp_session_id, &model_state)
            .await;

        let prompt_fut = client.prompt(&acp_session_id, &req.prompt);
        tokio::pin!(prompt_fut);

        tokio::select! {
            _ = &mut cancel_rx => {
                let _ = client.cancel(&acp_session_id).await;
                self.events.publish(SameEvent::SessionCancelled {
                    session_id: req.session_id.clone(),
                });
                self.events.publish(SameEvent::SessionStatusChanged {
                    session_id: req.session_id,
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
                    session_id: req.session_id,
                    status: SessionStatus::Completed,
                });
                Ok(())
            }
        }
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
    ) -> Result<(Arc<AcpClient>, String, SharedModelState)> {
        {
            let guard = self.acp_sessions.lock().await;
            if let Some(handle) = guard.get(&req.session_id) {
                if handle.provider_id == req.provider.id && handle.target == target {
                    return Ok((
                        handle.client.clone(),
                        handle.acp_session_id.clone(),
                        handle.model_state.clone(),
                    ));
                }
            }
        }

        let (client, mut acp_rx) = AcpClient::spawn(AcpClientConfig {
            command: req.provider.command.clone(),
            args: req.provider.args.clone(),
            env: req.provider.env.clone(),
            cwd: req.cwd.clone(),
            auto_approve: req.auto_approve,
            target: target.clone(),
        })
        .await?;

        let client = Arc::new(client);
        let model_state: SharedModelState = Arc::new(Mutex::new(None));

        // Forward ACP events into SAME EventBus
        {
            let events = self.events.clone();
            let responders = self.permission_responders.clone();
            let acp_sessions = self.acp_sessions.clone();
            let model_state = model_state.clone();
            let same_session_id = req.session_id.clone();
            let provider_id = req.provider.id.clone();

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
                            responder,
                            ..
                        } => {
                            responders
                                .lock()
                                .await
                                .insert(request_id.clone(), responder);
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
                            acp_sessions.lock().await.remove(&same_session_id);
                            break;
                        }
                    }
                }
            });
        }

        let loaded = match req.external_session_id.as_deref() {
            Some(ext_id) => client
                .load_session(ext_id, &req.cwd)
                .await
                .ok()
                .map(|state| (ext_id.to_string(), state)),
            None => None,
        };
        let (acp_session_id, initial_models) = match loaded {
            Some(found) => found,
            None => {
                let res = client.new_session(&req.cwd).await?;
                if let Some(state) = &res.model_state {
                    self.record_acp_defaults(&req.provider.id, state).await;
                }
                (res.session_id, res.model_state)
            }
        };
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

        self.acp_sessions.lock().await.insert(
            req.session_id.clone(),
            AcpSessionHandle {
                provider_id: req.provider.id.clone(),
                target,
                client: client.clone(),
                acp_session_id: acp_session_id.clone(),
                model_state: model_state.clone(),
            },
        );

        Ok((client, acp_session_id, model_state))
    }

    pub async fn cancel_session(&self, session_id: &str) {
        if let Some(tx) = self.cancellers.lock().await.remove(session_id) {
            let _ = tx.send(());
        }
    }

    pub async fn respond_permission(&self, request_id: &str, option_id: Option<String>) {
        if let Some(responder) = self.permission_responders.lock().await.remove(request_id) {
            match option_id {
                Some(id) => responder.respond(PermissionDecision::Selected { option_id: id }),
                None => responder.respond(PermissionDecision::Cancelled),
            }
        }
    }

    pub async fn shutdown_all(&self) {
        let mut guard = self.acp_sessions.lock().await;
        for (_, handle) in guard.drain() {
            handle.client.shutdown().await;
        }
    }
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
            auto_approve: false,
            exec: ExecPreference::Auto,
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
                auto_approve: true,
                exec: ExecPreference::Auto,
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

        let mut windows_agy = get("antigravity");
        windows_agy.id = "agy-win".into();
        windows_agy.command = r"C:\Users\me\AppData\Local\agy\bin\agy.exe".into();
        assert!(AgentManager::install_script(&windows_agy).is_some());
        assert_eq!(
            AgentManager::login_script(&windows_agy).windows,
            r"& 'C:\Users\me\AppData\Local\agy\bin\agy.exe' 'acp' '--login'"
        );
    }
}
