//! Parallel sessions UI state, cross-project background notifications,
//! Context Handoff confirmation, and Git Worktree / ReadOnly workdir management.

use super::*;
use same_core::{
    create_worktree, current_branch, default_worktree_path, has_uncommitted_changes, is_git_repo,
    merge_worktree_branch, remove_worktree, repo_root, unique_worktree_branch, HandoffMode,
    SessionWorkdir, WorkdirMode,
};

#[derive(Default, Clone)]
pub(super) struct SessionUiState {
    pub draft_prompt: String,
    pub attachments: Vec<PathBuf>,
    pub pasted_attachments: HashSet<PathBuf>,
    pub selected_prompt_skills: Vec<skills::PromptSkillRef>,
    pub history_recall: Option<(usize, String)>,
    pub selected_tool_turn_id: Option<String>,
    pub scroll_logical_top: Option<gpui::ListOffset>,
    pub has_unread_completion: bool,
    pub has_unread_error: bool,
    pub handoff_mode: Option<HandoffMode>,
    /// True only for the first turn after the user explicitly chose Fresh handoff.
    pub pending_fresh_handoff: bool,
}

#[derive(Clone)]
pub(super) struct ActiveRunInfo {
    pub run_id: String,
    pub provider_session_id: Option<String>,
    pub exec_mode_key: String,
    pub workdir: String,
}

#[derive(Clone)]
pub(super) struct BackgroundAlert {
    pub id: String,
    pub session_id: String,
    pub project_id: String,
    pub kind: NoteKind,
    pub title: String,
    pub detail: String,
}

#[derive(Clone)]
pub(super) struct PendingProviderSwitch {
    pub session_id: String,
    pub from_provider_id: String,
    pub to_provider_id: String,
    pub mode: HandoffMode,
    pub has_existing_provider_session: bool,
    pub has_attachments: bool,
    pub attachments_accessible: bool,
}

#[derive(Clone)]
pub(super) enum PendingWorktreeConfirm {
    Merge {
        source_dir: PathBuf,
        branch: String,
        target_dir: PathBuf,
        target_branch: String,
    },
    Remove {
        session_id: String,
        path: PathBuf,
        is_dirty: bool,
    },
}

pub(super) fn handoff_mode_label(lang: Language, mode: HandoffMode) -> &'static str {
    match mode {
        HandoffMode::Smart => t(lang, "Smart (推奨)", "Smart (Default)"),
        HandoffMode::Full => t(lang, "Full (全履歴)", "Full History"),
        HandoffMode::Fresh => t(lang, "Fresh (新規開始)", "Fresh Context"),
    }
}

pub(super) fn workdir_mode_label(lang: Language, mode: WorkdirMode) -> &'static str {
    match mode {
        WorkdirMode::Shared => t(lang, "共有 (Shared)", "Shared"),
        WorkdirMode::Isolated => t(lang, "分離 (Worktree)", "Isolated (Worktree)"),
        WorkdirMode::ReadOnly => t(lang, "読取専用 (Read Only)", "Read Only"),
    }
}

/// If `project_dir` is a subdirectory of its Git repository root, maps it to
/// the matching subdirectory inside `worktree_root`.
fn preserve_project_subdir(project_dir: &Path, worktree_root: &Path) -> PathBuf {
    let Some(root) = repo_root(project_dir) else {
        return worktree_root.to_path_buf();
    };
    let canon_proj = project_dir
        .canonicalize()
        .unwrap_or_else(|_| project_dir.to_path_buf());
    let canon_root = root.canonicalize().unwrap_or(root);
    if let Ok(rel) = canon_proj.strip_prefix(&canon_root) {
        if !rel.as_os_str().is_empty() {
            let candidate = worktree_root.join(rel);
            if candidate.exists() {
                return candidate;
            }
        }
    }
    worktree_root.to_path_buf()
}

impl SameAppView {
    /// Saves the currently active session's composer, attachments, skills, and scroll state.
    pub(super) fn save_session_ui_state(&mut self, sid: &str, cx: &Context<Self>) {
        let draft_prompt = self.prompt_input.read(cx).value().to_string();
        let scroll_top = self.feed.logical_scroll_top();
        let entry = self.session_ui_states.entry(sid.to_string()).or_default();
        entry.draft_prompt = draft_prompt;
        entry.attachments = std::mem::take(&mut self.attachments);
        entry.pasted_attachments = std::mem::take(&mut self.pasted_attachments);
        entry.selected_prompt_skills = std::mem::take(&mut self.selected_prompt_skills);
        entry.history_recall = self.history_recall.take();
        entry.selected_tool_turn_id = self.selected_tool_turn_id.take();
        entry.scroll_logical_top = Some(scroll_top);
    }

    /// Restores the target session's composer, attachments, skills, and clears unread flags.
    pub(super) fn restore_session_ui_state(
        &mut self,
        sid: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::ListOffset> {
        let state = self.session_ui_states.get_mut(sid);
        let (draft, attachments, pasted, prompt_skills, recall, tool_turn, scroll_top) =
            if let Some(st) = state {
                st.has_unread_completion = false;
                st.has_unread_error = false;
                (
                    st.draft_prompt.clone(),
                    std::mem::take(&mut st.attachments),
                    std::mem::take(&mut st.pasted_attachments),
                    std::mem::take(&mut st.selected_prompt_skills),
                    st.history_recall.take(),
                    st.selected_tool_turn_id.take(),
                    st.scroll_logical_top,
                )
            } else {
                (
                    String::new(),
                    Vec::new(),
                    HashSet::new(),
                    Vec::new(),
                    None,
                    None,
                    None,
                )
            };

        self.attachments = attachments;
        self.pasted_attachments = pasted;
        self.selected_prompt_skills = prompt_skills;
        self.history_recall = recall;
        self.selected_tool_turn_id = tool_turn;
        self.prompt_empty = draft.trim().is_empty();
        self.prompt_is_command = draft.starts_with('/') && !draft.contains('\n');
        self.prompt_input
            .update(cx, |ta, cx| ta.set_value(draft, window, cx));
        self.background_alerts.retain(|a| a.session_id != sid);
        scroll_top
    }

    /// Resolves a session's display title even when it belongs to another project.
    pub(super) fn session_title_by_id(&self, session_id: &str) -> String {
        if let Some(s) = self.sessions.iter().find(|s| s.id == session_id) {
            return self.session_title(s);
        }
        if let Ok(Some(s)) = self.storage.get_session(session_id) {
            return self.session_title(&s);
        }
        String::new()
    }

    pub(super) fn push_background_alert(
        &mut self,
        session_id: &str,
        kind: NoteKind,
        detail: String,
    ) {
        if self.selected_session_id.as_deref() == Some(session_id)
            && self.active_view == MainView::Chat
        {
            return;
        }
        let sess = self
            .sessions
            .iter()
            .find(|s| s.id == session_id)
            .cloned()
            .or_else(|| self.storage.get_session(session_id).ok().flatten());
        let Some(sess) = sess else {
            return;
        };
        let title = self.session_title(&sess);
        self.background_alerts
            .retain(|a| !(a.session_id == session_id && a.detail == detail));
        self.background_alerts.push(BackgroundAlert {
            id: Uuid::new_v4().to_string(),
            session_id: session_id.to_string(),
            project_id: sess.project_id,
            kind,
            title,
            detail,
        });
        if self.background_alerts.len() > 6 {
            let drain = self.background_alerts.len() - 6;
            self.background_alerts.drain(0..drain);
        }
    }

    /// Jumps directly to any session (switching projects automatically if needed).
    pub(super) fn jump_to_session(
        &mut self,
        project_id: Option<String>,
        session_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target_pid = project_id.or_else(|| {
            self.active_session_meta
                .get(&session_id)
                .map(|s| s.project_id.clone())
                .or_else(|| {
                    self.storage
                        .get_session(&session_id)
                        .ok()
                        .flatten()
                        .map(|s| s.project_id)
                })
        });
        if let Some(pid) = target_pid {
            if self.selected_project_id.as_deref() != Some(pid.as_str()) {
                self.select_project_with_session(pid, Some(session_id), window, cx);
                self.prompt_input.update(cx, |s, cx| s.focus(window, cx));
                return;
            }
        }
        self.select_session(session_id, window, cx);
    }

    /// Cancels a specific session by ID without affecting any other running sessions.
    pub(super) fn cancel_session_by_id(&mut self, sid: String) {
        if !self.is_session_running(&sid) {
            return;
        }
        let perms: Vec<String> = self
            .pending_permissions
            .iter()
            .filter(|p| p.session_id == sid)
            .map(|p| p.request_id.clone())
            .collect();
        self.pending_permissions.retain(|p| p.session_id != sid);
        let inputs: Vec<String> = self
            .pending_inputs
            .iter()
            .filter(|p| p.request.session_id == sid)
            .map(|p| p.request.request_id.clone())
            .collect();
        self.pending_inputs.retain(|p| p.request.session_id != sid);
        if self.is_selected_session(&sid) {
            self.mark_feed_tail_dirty();
        }
        let mgr = self.agent_manager.clone();
        self.rt.spawn(async move {
            for rid in perms {
                mgr.respond_permission(&rid, None).await;
            }
            for rid in inputs {
                mgr.respond_user_input(&rid, None).await;
            }
            mgr.cancel_session(&sid).await;
        });
    }

    /// Returns the effective working directory for a session (worktree path if
    /// bound, else the owning project's workdir).
    pub(super) fn effective_workdir_for_session(&self, sid: &str) -> Option<PathBuf> {
        if let Some(sw) = self.session_workdirs.get(sid) {
            return Some(PathBuf::from(&sw.path));
        }
        // `self.session_workdirs` is preloaded for all sessions in the current
        // project, so only hit SQLite for sessions belonging to other projects.
        let in_current_project = self.sessions.iter().any(|s| s.id == sid);
        if !in_current_project {
            if let Ok(Some(sw)) = self.storage.get_session_workdir(sid) {
                return Some(PathBuf::from(&sw.path));
            }
        }
        let project_id = self
            .sessions
            .iter()
            .find(|s| s.id == sid)
            .map(|s| s.project_id.clone())
            .or_else(|| {
                self.active_session_meta
                    .get(sid)
                    .map(|s| s.project_id.clone())
            })
            .or_else(|| {
                self.storage
                    .get_session(sid)
                    .ok()
                    .flatten()
                    .map(|s| s.project_id)
            })?;
        self.projects
            .iter()
            .find(|p| p.id == project_id)
            .map(|p| PathBuf::from(&p.workdir))
            .or_else(|| {
                self.storage
                    .get_project(&project_id)
                    .ok()
                    .flatten()
                    .map(|p| PathBuf::from(&p.workdir))
            })
    }

    pub(super) fn session_workdir_record(&self, sid: &str) -> Option<SessionWorkdir> {
        self.session_workdirs.get(sid).cloned()
    }

    pub(super) fn session_workdir_mode(&self, sid: &str) -> WorkdirMode {
        self.session_workdirs
            .get(sid)
            .map(|sw| sw.mode)
            .unwrap_or(WorkdirMode::Shared)
    }

    pub(super) fn session_handoff_mode(&self, sid: &str) -> HandoffMode {
        self.sessions
            .iter()
            .find(|s| s.id == sid)
            .and_then(|s| s.handoff_mode)
            .or_else(|| self.session_ui_states.get(sid).and_then(|s| s.handoff_mode))
            .unwrap_or(self.config.default_handoff_mode)
    }

    pub(super) fn exec_mode_key(&self) -> &'static str {
        match self.exec_mode() {
            ExecMode::Auto => "auto",
            ExecMode::Host => "host",
            ExecMode::Wsl => "wsl",
        }
    }

    /// Initiates a provider switch. When the session already has messages,
    /// prompts the user to confirm sending context to another provider and
    /// choose the handoff mode.
    pub(super) fn request_provider_switch(&mut self, sid: &str, to_provider_id: &str) {
        let lang = self.config.language;
        if self.is_session_running(sid) {
            self.note(
                NoteKind::Warning,
                t(
                    lang,
                    "実行中のターンが完了または停止してからプロバイダを切り替えてください",
                    "Wait for the running turn to finish or stop it before switching providers",
                ),
            );
            return;
        }
        let Some(from_pid) = self
            .sessions
            .iter()
            .find(|s| s.id == sid)
            .map(|s| s.provider_id.clone())
        else {
            return;
        };
        if from_pid == to_provider_id {
            return;
        }
        let msgs = self.storage.list_messages(sid).unwrap_or_default();
        if msgs.is_empty() {
            // No conversation history yet: switch immediately without confirmation.
            self.switch_session_provider(sid, to_provider_id);
            return;
        }
        let workdir = self
            .effective_workdir_for_session(sid)
            .map(|p| simplify_path(&p.to_string_lossy()))
            .unwrap_or_default();
        let has_existing = self
            .storage
            .get_active_provider_session(sid, to_provider_id, self.exec_mode_key(), &workdir)
            .ok()
            .flatten()
            .is_some();
        let has_attachments = !self.attachments.is_empty();
        let attachments_accessible = self.attachments.iter().all(|p| p.exists());
        self.pending_provider_switch = Some(PendingProviderSwitch {
            session_id: sid.to_string(),
            from_provider_id: from_pid,
            to_provider_id: to_provider_id.to_string(),
            mode: self.session_handoff_mode(sid),
            has_existing_provider_session: has_existing,
            has_attachments,
            attachments_accessible,
        });
    }

    pub(super) fn confirm_provider_switch(&mut self) {
        let Some(sw) = self.pending_provider_switch.take() else {
            return;
        };
        if self.is_session_running(&sw.session_id) {
            self.note(
                NoteKind::Warning,
                t(
                    self.config.language,
                    "実行中のターンが完了または停止してからプロバイダを切り替えてください",
                    "Wait for the running turn to finish or stop it before switching providers",
                ),
            );
            return;
        }
        let entry = self
            .session_ui_states
            .entry(sw.session_id.clone())
            .or_default();
        entry.handoff_mode = Some(sw.mode);
        entry.pending_fresh_handoff = sw.mode == HandoffMode::Fresh;
        // Persist the chosen handoff mode (especially Fresh) and record the
        // message cutoff boundary so pre-Fresh history is never disclosed even
        // on retries or subsequent native-session fallbacks.
        let _ = self
            .storage
            .set_session_handoff_mode(&sw.session_id, Some(sw.mode));
        let fresh_cutoff = if sw.mode == HandoffMode::Fresh {
            self.storage
                .list_messages(&sw.session_id)
                .ok()
                .and_then(|ms| ms.last().map(|m| m.id.clone()))
        } else {
            None
        };
        if sw.mode == HandoffMode::Fresh {
            let _ = self
                .storage
                .set_session_handoff_cutoff(&sw.session_id, fresh_cutoff.as_deref());
        }
        if let Some(s) = self.sessions.iter_mut().find(|x| x.id == sw.session_id) {
            s.handoff_mode = Some(sw.mode);
            if sw.mode == HandoffMode::Fresh {
                s.handoff_cutoff_message_id = fresh_cutoff;
            }
        }

        // Look up whether a reusable ProviderSession already exists for the target provider.
        let workdir = self
            .effective_workdir_for_session(&sw.session_id)
            .map(|p| simplify_path(&p.to_string_lossy()))
            .unwrap_or_default();
        let existing_ext = if sw.mode == HandoffMode::Fresh {
            // Invalidate ALL active native sessions for the target provider
            // across every workdir/exec_mode so pre-Fresh threads can never be resumed.
            let _ = self
                .storage
                .invalidate_provider_sessions_for_provider(&sw.session_id, &sw.to_provider_id);
            None
        } else {
            self.storage
                .get_active_provider_session(
                    &sw.session_id,
                    &sw.to_provider_id,
                    self.exec_mode_key(),
                    &workdir,
                )
                .ok()
                .flatten()
                .map(|ps| ps.external_session_id)
        };

        let _ = self.storage.update_session_meta(
            &sw.session_id,
            None,
            Some(&sw.to_provider_id),
            Some(None),
            Some(existing_ext.as_deref()),
            None,
        );
        if let Some(s) = self.sessions.iter_mut().find(|x| x.id == sw.session_id) {
            s.provider_id = sw.to_provider_id.clone();
            s.external_session_id = existing_ext.clone();
            s.model = None;
            s.effort = None;
        }
        let _ = self.storage.set_session_effort(&sw.session_id, None);
        self.agent_current_model.remove(&sw.session_id);
        self.slash_commands.remove(&sw.session_id);
        self.agent_manager.release_session(&sw.session_id);

        let lang = self.config.language;
        let to_name = self.provider_name(&sw.to_provider_id);
        let msg = if existing_ext.is_some() {
            match lang {
                Language::Ja => format!(
                    "{to_name} へ切り替えました（既存ネイティブセッションを再開＋差分同期）"
                ),
                Language::En => {
                    format!("Switched to {to_name} (resuming native session with delta sync)")
                }
            }
        } else {
            match lang {
                Language::Ja => format!(
                    "{to_name} へ切り替えました（引き継ぎ方式: {}）",
                    handoff_mode_label(lang, sw.mode)
                ),
                Language::En => format!(
                    "Switched to {to_name} (handoff: {})",
                    handoff_mode_label(lang, sw.mode)
                ),
            }
        };
        self.note(NoteKind::Info, msg);
    }

    pub(super) fn apply_default_workdir_mode(&mut self, sid: &str, project_workdir: &str) {
        let mode = self.config.default_workdir_mode;
        match mode {
            WorkdirMode::Shared => {}
            WorkdirMode::ReadOnly => {
                let branch = self
                    .cached_branch_for_path(Path::new(project_workdir))
                    .or_else(|| current_branch(Path::new(project_workdir)));
                let sw = SessionWorkdir {
                    session_id: sid.to_string(),
                    mode: WorkdirMode::ReadOnly,
                    path: project_workdir.to_string(),
                    base_branch: branch.clone(),
                    branch_name: branch,
                    is_worktree: false,
                    created_at: Utc::now(),
                };
                let _ = self.storage.upsert_session_workdir(&sw);
                self.session_workdirs.insert(sid.to_string(), sw);
            }
            WorkdirMode::Isolated => {
                let repo = PathBuf::from(project_workdir);
                if !is_git_repo(&repo) {
                    // Non-Git project: gracefully stay in Shared mode.
                    return;
                }
                let base_slug = format!("same-{}", &sid[..sid.len().min(8)]);
                let slug = unique_worktree_branch(&repo, &base_slug);
                let base = current_branch(&repo);
                let wt_path = default_worktree_path(&repo, &slug);
                match create_worktree(&repo, &wt_path, &slug, base.as_deref()) {
                    Ok(info) => {
                        let effective_path = preserve_project_subdir(&repo, &info.path);
                        let sw = SessionWorkdir {
                            session_id: sid.to_string(),
                            mode: WorkdirMode::Isolated,
                            path: simplify_path(&effective_path.to_string_lossy()),
                            base_branch: base,
                            branch_name: info.branch,
                            is_worktree: true,
                            created_at: Utc::now(),
                        };
                        let _ = self.storage.upsert_session_workdir(&sw);
                        self.session_workdirs.insert(sid.to_string(), sw);
                    }
                    Err(err) => {
                        // Do not silently write into the shared checkout when
                        // Isolated was requested: fall back to ReadOnly and warn.
                        let sw = SessionWorkdir {
                            session_id: sid.to_string(),
                            mode: WorkdirMode::ReadOnly,
                            path: project_workdir.to_string(),
                            base_branch: base.clone(),
                            branch_name: base,
                            is_worktree: false,
                            created_at: Utc::now(),
                        };
                        let _ = self.storage.upsert_session_workdir(&sw);
                        self.session_workdirs.insert(sid.to_string(), sw);
                        let lang = self.config.language;
                        self.note(
                            NoteKind::Warning,
                            match lang {
                                Language::Ja => format!(
                                    "Worktree の自動作成に失敗したため読取専用で開始しました（{err:#}）。共有へ切り替える場合はヘッダーから選択してください。"
                                ),
                                Language::En => format!(
                                    "Failed to create isolated worktree ({err:#}); started in Read Only to protect the shared checkout."
                                ),
                            },
                        );
                    }
                }
            }
        }
    }

    pub(super) fn set_session_workdir_mode(
        &mut self,
        sid: &str,
        mode: WorkdirMode,
        cx: &Context<Self>,
    ) {
        let lang = self.config.language;
        if self.is_session_running(sid) {
            self.note(
                NoteKind::Warning,
                t(
                    lang,
                    "実行中は作業環境を変更できません",
                    "Cannot change working directory mode while running",
                ),
            );
            return;
        }
        let Some(project) = self.current_project().cloned() else {
            return;
        };
        match mode {
            WorkdirMode::Shared | WorkdirMode::ReadOnly => {
                if mode == WorkdirMode::ReadOnly {
                    if let Some(sess) = self.sessions.iter().find(|s| s.id == sid) {
                        let caps = self
                            .config
                            .find_provider(&sess.provider_id)
                            .map(|p| AgentManager::capabilities_for(p.kind));
                        if caps.is_some_and(|c| !c.supports_read_only) {
                            self.note(
                                NoteKind::Warning,
                                t(
                                    lang,
                                    "このプロバイダは完全な Read Only 制御を保証できません",
                                    "This provider cannot guarantee strict Read Only enforcement",
                                ),
                            );
                        }
                    }
                }
                let branch = self
                    .cached_branch_for_path(Path::new(&project.workdir))
                    .or_else(|| current_branch(Path::new(&project.workdir)));
                let sw = SessionWorkdir {
                    session_id: sid.to_string(),
                    mode,
                    path: project.workdir.clone(),
                    base_branch: branch.clone(),
                    branch_name: branch,
                    is_worktree: false,
                    created_at: Utc::now(),
                };
                let _ = self.storage.upsert_session_workdir(&sw);
                self.session_workdirs.insert(sid.to_string(), sw);
                self.agent_manager.release_session(sid);
                self.reload_workspace_files(cx);
            }
            WorkdirMode::Isolated => {
                let repo = PathBuf::from(&project.workdir);
                if !is_git_repo(&repo) {
                    self.note(
                        NoteKind::Warning,
                        t(
                            lang,
                            "Git リポジトリではないため Worktree を作成できません（Shared / Read Only は利用可能です）",
                            "Not a Git repository: cannot create a worktree (Shared / Read Only are available)",
                        ),
                    );
                    return;
                }
                let base_slug = format!("same-{}", &sid[..sid.len().min(8)]);
                let slug = unique_worktree_branch(&repo, &base_slug);
                let base = current_branch(&repo);
                let wt_path = default_worktree_path(&repo, &slug);
                match create_worktree(&repo, &wt_path, &slug, base.as_deref()) {
                    Ok(info) => {
                        let effective_path = preserve_project_subdir(&repo, &info.path);
                        let sw = SessionWorkdir {
                            session_id: sid.to_string(),
                            mode: WorkdirMode::Isolated,
                            path: simplify_path(&effective_path.to_string_lossy()),
                            base_branch: base,
                            branch_name: info.branch.clone(),
                            is_worktree: true,
                            created_at: Utc::now(),
                        };
                        let _ = self.storage.upsert_session_workdir(&sw);
                        self.session_workdirs.insert(sid.to_string(), sw);
                        self.agent_manager.release_session(sid);
                        self.reload_workspace_files(cx);
                        self.note(
                            NoteKind::Success,
                            match lang {
                                Language::Ja => format!("Worktree を作成しました: {slug}"),
                                Language::En => format!("Created worktree: {slug}"),
                            },
                        );
                    }
                    Err(err) => {
                        self.note(NoteKind::Error, format!("{err:#}"));
                    }
                }
            }
        }
    }

    pub(super) fn bind_existing_worktree(
        &mut self,
        sid: &str,
        wt_path: PathBuf,
        branch: Option<String>,
        cx: &Context<Self>,
    ) {
        let lang = self.config.language;
        if self.is_session_running(sid) {
            self.note(
                NoteKind::Warning,
                t(
                    lang,
                    "実行中は作業環境を変更できません",
                    "Cannot change worktree while running",
                ),
            );
            return;
        }
        let base_branch = self
            .current_project()
            .and_then(|p| current_branch(Path::new(&p.workdir)));
        let sw = SessionWorkdir {
            session_id: sid.to_string(),
            mode: WorkdirMode::Isolated,
            path: simplify_path(&wt_path.to_string_lossy()),
            base_branch,
            branch_name: branch.clone(),
            is_worktree: true,
            created_at: Utc::now(),
        };
        let _ = self.storage.upsert_session_workdir(&sw);
        self.session_workdirs.insert(sid.to_string(), sw);
        self.agent_manager.release_session(sid);
        self.reload_workspace_files(cx);
        self.note(
            NoteKind::Info,
            match lang {
                Language::Ja => format!(
                    "既存 Worktree に切り替えました: {}",
                    branch.as_deref().unwrap_or("detached")
                ),
                Language::En => format!(
                    "Bound existing worktree: {}",
                    branch.as_deref().unwrap_or("detached")
                ),
            },
        );
    }

    pub(super) fn execute_pending_worktree_confirm(&mut self, cx: &Context<Self>) {
        let Some(confirm) = self.pending_worktree_confirm.take() else {
            return;
        };
        let lang = self.config.language;
        let Some(project) = self.current_project().cloned() else {
            return;
        };
        let repo = PathBuf::from(&project.workdir);
        match confirm {
            PendingWorktreeConfirm::Merge {
                source_dir,
                branch,
                target_dir,
                target_branch,
            } => {
                let actual_source =
                    current_branch(&source_dir).unwrap_or_else(|| "HEAD".to_string());
                if actual_source != branch {
                    self.note(
                        NoteKind::Warning,
                        match lang {
                            Language::Ja => format!(
                                "Worktree のブランチが '{branch}' から '{actual_source}' へ変わっています。再度確認してください。"
                            ),
                            Language::En => format!(
                                "Source worktree branch changed from '{branch}' to '{actual_source}'. Please confirm again."
                            ),
                        },
                    );
                    return;
                }
                let actual_branch =
                    current_branch(&target_dir).unwrap_or_else(|| "HEAD".to_string());
                if actual_branch != target_branch {
                    self.note(
                        NoteKind::Warning,
                        match lang {
                            Language::Ja => format!(
                                "マージ先ブランチが '{target_branch}' から '{actual_branch}' へ変わっています。再度確認してください。"
                            ),
                            Language::En => format!(
                                "Target branch changed from '{target_branch}' to '{actual_branch}'. Please confirm again."
                            ),
                        },
                    );
                    return;
                }
                match merge_worktree_branch(&target_dir, &branch) {
                    Ok(_) => {
                        self.note(
                            NoteKind::Success,
                            match lang {
                                Language::Ja => {
                                    format!(
                                        "ブランチ '{branch}' を '{target_branch}' へマージしました"
                                    )
                                }
                                Language::En => {
                                    format!("Merged branch '{branch}' into '{target_branch}'")
                                }
                            },
                        );
                        self.reload_workspace_files(cx);
                    }
                    Err(err) => self.note(NoteKind::Error, format!("{err:#}")),
                }
            }
            PendingWorktreeConfirm::Remove {
                session_id,
                path,
                is_dirty,
            } => {
                if self.is_session_running(&session_id) {
                    self.note(
                        NoteKind::Warning,
                        t(
                            lang,
                            "セッションが実行中のため Worktree を削除できません",
                            "Cannot remove worktree while its session is running",
                        ),
                    );
                    return;
                }
                if is_dirty {
                    self.note(
                        NoteKind::Warning,
                        t(
                            lang,
                            "未コミットの変更があるため Worktree を削除しませんでした（変更を保護しています）",
                            "Worktree has uncommitted changes and was not removed",
                        ),
                    );
                    return;
                }
                let path_str = simplify_path(&path.to_string_lossy());
                let current_bound = self
                    .session_workdir_record(&session_id)
                    .map(|sw| simplify_path(&sw.path));
                if current_bound.as_deref() != Some(path_str.as_str()) {
                    self.note(
                        NoteKind::Warning,
                        t(
                            lang,
                            "セッションの作業環境が既に変更されているため、削除を中止しました",
                            "Session working directory has changed since confirmation; removal aborted",
                        ),
                    );
                    return;
                }
                let wt_root = repo_root(&path).unwrap_or_else(|| path.clone());
                let wt_root_str = simplify_path(&wt_root.to_string_lossy());
                let used_as_project_root = self.projects.iter().any(|p| {
                    let pw = simplify_path(&p.workdir);
                    pw == path_str || pw == wt_root_str
                });
                if used_as_project_root {
                    self.note(
                        NoteKind::Warning,
                        t(
                            lang,
                            "この Worktree は別プロジェクトのルートディレクトリとして登録されているため削除できません",
                            "Cannot remove worktree because it is registered as a project root",
                        ),
                    );
                    return;
                }
                let other_users: Vec<String> = self
                    .storage
                    .list_sessions_using_workdir(&path_str)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|id| id != &session_id)
                    .collect();
                if !other_users.is_empty() {
                    self.note(
                        NoteKind::Warning,
                        match lang {
                            Language::Ja => format!(
                                "他のセッション（{} 件）がこの Worktree を使用中のため削除できません",
                                other_users.len()
                            ),
                            Language::En => format!(
                                "Cannot remove worktree while {} other session(s) are using it",
                                other_users.len()
                            ),
                        },
                    );
                    return;
                }
                match remove_worktree(&repo, &wt_root, false) {
                    Ok(()) => {
                        let _ = self.storage.delete_session_workdir(&session_id);
                        self.session_workdirs.remove(&session_id);
                        self.agent_manager.release_session(&session_id);
                        self.reload_workspace_files(cx);
                        self.note(
                            NoteKind::Success,
                            t(lang, "Worktree を削除しました", "Removed worktree"),
                        );
                    }
                    Err(err) => self.note(NoteKind::Error, format!("{err:#}")),
                }
            }
        }
    }

    /// Sidebar section listing all running or waiting sessions across all projects.
    pub(super) fn render_running_sessions_section(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let mut active_ids: Vec<String> = self.live_drafts.keys().cloned().collect();
        for p in &self.pending_permissions {
            if !active_ids.contains(&p.session_id) {
                active_ids.push(p.session_id.clone());
            }
        }
        for p in &self.pending_inputs {
            if !active_ids.contains(&p.request.session_id) {
                active_ids.push(p.request.session_id.clone());
            }
        }
        if active_ids.is_empty() {
            return None;
        }
        active_ids.sort();

        let theme = cx.theme().clone();
        let lang = self.config.language;
        let header_label = match (lang, self.config.max_concurrent_sessions()) {
            (Language::Ja, Some(max_c)) => format!("実行中 ({}/{max_c})", active_ids.len()),
            (Language::Ja, None) => format!("実行中 ({})", active_ids.len()),
            (Language::En, Some(max_c)) => format!("ACTIVE ({}/{max_c})", active_ids.len()),
            (Language::En, None) => format!("ACTIVE ({})", active_ids.len()),
        };

        Some(
            div()
                .v_flex()
                .gap_1()
                .child(section_label(&header_label, &theme))
                .child(
                    div()
                        .id("sidebar-active-sessions-scroll")
                        .v_flex()
                        .max_h(px(140.0))
                        .overflow_y_scroll()
                        .gap(px(2.0))
                        .children(active_ids.into_iter().map(|sid| {
                            let sess = self
                                .sessions
                                .iter()
                                .find(|s| s.id == sid)
                                .or_else(|| self.active_session_meta.get(&sid));
                            let title = sess
                                .map(|s| self.session_title(s))
                                .unwrap_or_else(|| sid[..sid.len().min(8)].to_string());
                            let pid = sess.map(|s| s.project_id.clone());
                            let provider_lbl = sess
                                .map(|s| self.provider_name(&s.provider_id))
                                .unwrap_or_default();
                            let waiting_perm =
                                self.pending_permissions.iter().any(|p| p.session_id == sid);
                            let waiting_input = self
                                .pending_inputs
                                .iter()
                                .any(|p| p.request.session_id == sid);
                            let dot: Hsla = if waiting_perm || waiting_input {
                                status_amber().into()
                            } else {
                                accent_indicator().into()
                            };
                            let status_txt = if waiting_perm {
                                t(lang, "承認待ち", "Approval")
                            } else if waiting_input {
                                t(lang, "質問待ち", "Input")
                            } else {
                                t(lang, "実行中", "Running")
                            };
                            let sid_jump = sid.clone();
                            let sid_stop = sid.clone();
                            let selected =
                                self.selected_session_id.as_deref() == Some(sid.as_str());

                            list_row(
                                SharedString::from(format!("active-sess-{sid}")),
                                selected,
                                &theme,
                            )
                            .child(div().size(px(7.0)).flex_shrink_0().rounded_full().bg(dot))
                            .child(
                                div()
                                    .v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .child(div().text_xs().truncate().child(title))
                                    .child(
                                        div()
                                            .text_xs()
                                            .truncate()
                                            .text_color(theme.muted_foreground)
                                            .child(format!("{status_txt} · {provider_lbl}")),
                                    ),
                            )
                            .child(
                                Button::new(SharedString::from(format!("active-stop-{sid}")))
                                    .xsmall()
                                    .ghost()
                                    .tooltip(t(lang, "このセッションを停止", "Stop session"))
                                    .label("■")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.cancel_session_by_id(sid_stop.clone());
                                        cx.notify();
                                    })),
                            )
                            .on_click(cx.listener(
                                move |this, _, window, cx| {
                                    this.jump_to_session(pid.clone(), sid_jump.clone(), window, cx);
                                    cx.notify();
                                },
                            ))
                        })),
                )
                .child(div().h(px(1.0)).bg(theme.border))
                .into_any_element(),
        )
    }

    /// Clickable background notification banner allowing direct jump to the session.
    pub(super) fn render_background_alerts_banner(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.background_alerts.is_empty() {
            return None;
        }
        let theme = cx.theme().clone();
        let lang = self.config.language;
        Some(
            div()
                .v_flex()
                .px_4()
                .py_1()
                .gap_1()
                .bg(theme.sidebar)
                .border_b_1()
                .border_color(theme.border)
                .children(self.background_alerts.iter().map(|alert| {
                    let aid = alert.id.clone();
                    let sid = alert.session_id.clone();
                    let pid = alert.project_id.clone();
                    let color: Hsla = match alert.kind {
                        NoteKind::Error => status_red().into(),
                        NoteKind::Warning => status_amber().into(),
                        NoteKind::Success => status_green().into(),
                        NoteKind::Info => accent_text().into(),
                    };
                    div()
                        .h_flex()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .child(
                            div()
                                .h_flex()
                                .items_center()
                                .gap_1()
                                .flex_1()
                                .min_w_0()
                                .child(div().size(px(6.0)).rounded_full().bg(color))
                                .child(
                                    div()
                                        .text_xs()
                                        .truncate()
                                        .child(format!("{}: {}", alert.title, alert.detail)),
                                ),
                        )
                        .child(
                            div()
                                .h_flex()
                                .items_center()
                                .gap_1()
                                .child(
                                    Button::new(SharedString::from(format!("alert-jump-{aid}")))
                                        .xsmall()
                                        .primary()
                                        .label(t(lang, "移動", "Open"))
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.jump_to_session(
                                                Some(pid.clone()),
                                                sid.clone(),
                                                window,
                                                cx,
                                            );
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    Button::new(SharedString::from(format!("alert-dismiss-{aid}")))
                                        .xsmall()
                                        .ghost()
                                        .label("✕")
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.background_alerts.retain(|a| a.id != aid);
                                            cx.notify();
                                        })),
                                ),
                        )
                }))
                .into_any_element(),
        )
    }

    /// Confirmation card when switching to a different provider with existing history.
    pub(super) fn render_provider_switch_confirm(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let sw = self.pending_provider_switch.as_ref()?;
        let theme = cx.theme().clone();
        let lang = self.config.language;
        let from_name = self.provider_name(&sw.from_provider_id);
        let to_name = self.provider_name(&sw.to_provider_id);
        let current_mode = sw.mode;

        let reuse_note = if sw.has_existing_provider_session {
            t(
                lang,
                "既存のネイティブセッションを再開し、未同期の差分のみ引き継ぎます。",
                "An existing native session will be resumed with delta sync only.",
            )
        } else {
            t(
                lang,
                "新しいプロバイダセッションを作成し、選択した方式でコンテキストを引き継ぎます。",
                "A new provider session will be created using the selected handoff mode.",
            )
        };

        Some(
            div()
                .v_flex()
                .mx_4()
                .mb_2()
                .p_3()
                .gap_2()
                .rounded(px(8.0))
                .bg(theme.sidebar)
                .border_1()
                .border_color(accent())
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::BOLD)
                        .child(match lang {
                            Language::Ja => format!(
                                "プロバイダ切り替え確認: {from_name} → {to_name}"
                            ),
                            Language::En => format!(
                                "Confirm Provider Switch: {from_name} → {to_name}"
                            ),
                        }),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(match lang {
                            Language::Ja => format!(
                                "この操作により、会話コンテキストが別の事業者（{to_name}）へ送信されます。内部思考ログは除外され、既知形式のキー・トークンはベストエフォートでマスクされますが、完全ではないため機密情報を含む場合は「Fresh」を選択してください。"
                            ),
                            Language::En => format!(
                                "Conversation context will be sent to another provider ({to_name}). Internal thinking logs are excluded and common secret patterns are redacted on a best-effort basis; choose Fresh if the history may contain sensitive credentials."
                            ),
                        }),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(accent_text())
                        .child(reuse_note),
                )
                .when(sw.has_attachments, |el| {
                    let note = if sw.attachments_accessible {
                        t(
                            lang,
                            "添付ファイルが設定されています。切り替え先プロバイダにもアクセス可能です。",
                            "Attachments are present and accessible for the target provider.",
                        )
                    } else {
                        t(
                            lang,
                            "⚠ 一部の添付ファイルが見つかりません。再送前に確認してください。",
                            "⚠ Some attached files are no longer accessible; verify before sending.",
                        )
                    };
                    el.child(div().text_xs().text_color(status_amber()).child(note))
                })
                .child(
                    div()
                        .h_flex()
                        .items_center()
                        .gap_1()
                        .flex_wrap()
                        .children(HandoffMode::ALL.map(|mode| {
                            Button::new(SharedString::from(format!(
                                "handoff-mode-{}",
                                mode.as_str()
                            )))
                            .xsmall()
                            .label(handoff_mode_label(lang, mode))
                            .when(current_mode == mode, |b| b.primary())
                            .when(current_mode != mode, |b| b.ghost())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(ref mut pending) = this.pending_provider_switch {
                                    pending.mode = mode;
                                }
                                cx.notify();
                            }))
                        })),
                )
                .child(
                    div()
                        .h_flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("btn-cancel-provider-switch")
                                .xsmall()
                                .ghost()
                                .label(t(lang, "キャンセル", "Cancel"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.pending_provider_switch = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new("btn-confirm-provider-switch")
                                .xsmall()
                                .primary()
                                .label(t(lang, "切り替えを確定", "Confirm Switch"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.confirm_provider_switch();
                                    cx.notify();
                                })),
                        ),
                )
                .into_any_element(),
        )
    }

    /// Confirmation banner for Git worktree merge/remove operations.
    pub(super) fn render_worktree_confirm_banner(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let confirm = self.pending_worktree_confirm.as_ref()?;
        let theme = cx.theme().clone();
        let lang = self.config.language;
        let (msg, action_lbl) = match confirm {
            PendingWorktreeConfirm::Merge {
                branch,
                target_branch,
                target_dir,
                ..
            } => (
                match lang {
                    Language::Ja => format!(
                        "Worktree ブランチ '{branch}' を '{target_branch}' ({}) へマージしますか？",
                        target_dir.display()
                    ),
                    Language::En => format!(
                        "Merge worktree branch '{branch}' into '{target_branch}' ({})?",
                        target_dir.display()
                    ),
                },
                t(lang, "マージ実行", "Merge"),
            ),
            PendingWorktreeConfirm::Remove { path, is_dirty, .. } => (
                if *is_dirty {
                    match lang {
                        Language::Ja => format!(
                            "⚠ '{}' には未コミットの変更があるため削除できません。",
                            path.display()
                        ),
                        Language::En => format!(
                            "⚠ '{}' has uncommitted changes and cannot be removed.",
                            path.display()
                        ),
                    }
                } else {
                    match lang {
                        Language::Ja => format!("Worktree '{}' を削除しますか？", path.display()),
                        Language::En => format!("Remove worktree '{}'?", path.display()),
                    }
                },
                t(lang, "削除実行", "Remove"),
            ),
        };
        let blocked = matches!(
            confirm,
            PendingWorktreeConfirm::Remove { is_dirty: true, .. }
        );

        Some(
            div()
                .h_flex()
                .items_center()
                .justify_between()
                .gap_2()
                .px_4()
                .py_1()
                .bg(theme.warning.opacity(0.14))
                .border_b_1()
                .border_color(theme.border)
                .child(div().text_xs().truncate().child(msg))
                .child(
                    div()
                        .h_flex()
                        .gap_1()
                        .child(
                            Button::new("btn-cancel-wt-confirm")
                                .xsmall()
                                .ghost()
                                .label(t(lang, "閉じる", "Cancel"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.pending_worktree_confirm = None;
                                    cx.notify();
                                })),
                        )
                        .when(!blocked, |el| {
                            el.child(
                                Button::new("btn-exec-wt-confirm")
                                    .xsmall()
                                    .primary()
                                    .label(action_lbl)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.execute_pending_worktree_confirm(cx);
                                        cx.notify();
                                    })),
                            )
                        }),
                )
                .into_any_element(),
        )
    }

    /// Chat header dropdown for Shared / Isolated (Worktree) / Read Only environment.
    pub(super) fn render_workdir_header_menu(
        &self,
        session: &Session,
        running: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let lang = self.config.language;
        let sid = session.id.clone();
        let sw = self.session_workdir_record(&sid);
        let mode = sw.as_ref().map(|w| w.mode).unwrap_or(WorkdirMode::Shared);
        let branch_badge = sw
            .as_ref()
            .and_then(|w| w.branch_name.clone())
            .filter(|_| sw.as_ref().is_some_and(|w| w.is_worktree));
        let label = match (mode, branch_badge.as_deref()) {
            (WorkdirMode::Isolated, Some(br)) => format!("⎇ {br} ▾"),
            (WorkdirMode::Isolated, None) => format!("⎇ {} ▾", t(lang, "Worktree", "Worktree")),
            (WorkdirMode::ReadOnly, _) => format!("🔒 {} ▾", t(lang, "読取専用", "Read Only")),
            (WorkdirMode::Shared, _) => format!("📁 {} ▾", t(lang, "共有", "Shared")),
        };

        let existing_worktrees = self.cached_worktrees.clone();
        let view = cx.entity().downgrade();
        let sw_clone = sw.clone();

        Button::new("chat-header-workdir")
            .xsmall()
            .ghost()
            .disabled(running)
            .tooltip(t(
                lang,
                "セッションの作業環境（共有 / Git Worktree 分離 / 読取専用）",
                "Session working environment (Shared / Isolated Git Worktree / Read Only)",
            ))
            .label(label)
            .dropdown_menu(move |menu, _, _| {
                let mut menu = menu;
                for m in WorkdirMode::ALL {
                    let v = view.clone();
                    let sid2 = sid.clone();
                    menu = menu.item(
                        PopupMenuItem::new(workdir_mode_label(lang, m))
                            .checked(mode == m)
                            .on_click(move |_, _, cx| {
                                let sid3 = sid2.clone();
                                let _ = v.update(cx, |this, cx| {
                                    this.set_session_workdir_mode(&sid3, m, cx);
                                    cx.notify();
                                });
                            }),
                    );
                }
                let secondary_wts: Vec<_> = existing_worktrees
                    .iter()
                    .filter(|w| !w.is_main)
                    .cloned()
                    .collect();
                if !secondary_wts.is_empty() {
                    menu = menu.item(PopupMenuItem::separator());
                    for wt in secondary_wts {
                        let v = view.clone();
                        let sid2 = sid.clone();
                        let wt_path = wt.path.clone();
                        let wt_branch = wt.branch.clone();
                        let dirty = if wt.is_dirty { " *" } else { "" };
                        let lbl =
                            format!("⎇ {}{dirty}", wt_branch.as_deref().unwrap_or("detached"));
                        menu = menu.item(PopupMenuItem::new(lbl).on_click(move |_, _, cx| {
                            let sid3 = sid2.clone();
                            let p = wt_path.clone();
                            let b = wt_branch.clone();
                            let _ = v.update(cx, |this, cx| {
                                this.bind_existing_worktree(&sid3, p, b, cx);
                                cx.notify();
                            });
                        }));
                    }
                }
                if let Some(ref cur_sw) = sw_clone {
                    if cur_sw.is_worktree {
                        menu = menu.item(PopupMenuItem::separator());
                        if cur_sw.branch_name.is_some() {
                            let v = view.clone();
                            let src_dir = PathBuf::from(&cur_sw.path);
                            let fallback_br = cur_sw.branch_name.clone().unwrap_or_default();
                            menu = menu.item(
                                PopupMenuItem::new(t(lang, "メインへマージ…", "Merge into main…"))
                                    .on_click(move |_, _, cx| {
                                        let src_dir2 = src_dir.clone();
                                        let fallback_br2 = fallback_br.clone();
                                        let _ = v.update(cx, |this, cx| {
                                            let actual_src_branch =
                                                current_branch(&src_dir2).unwrap_or(fallback_br2);
                                            let main_wt = this
                                                .cached_worktrees
                                                .iter()
                                                .find(|w| w.is_main)
                                                .cloned()
                                                .or_else(|| {
                                                    this.current_project()
                                                        .and_then(|p| {
                                                            same_core::list_worktrees(Path::new(
                                                                &p.workdir,
                                                            ))
                                                            .ok()
                                                        })
                                                        .and_then(|wts| {
                                                            wts.into_iter().find(|w| w.is_main)
                                                        })
                                                });
                                            let target_dir = main_wt
                                                .as_ref()
                                                .map(|w| w.path.clone())
                                                .or_else(|| {
                                                    this.current_project()
                                                        .map(|p| PathBuf::from(&p.workdir))
                                                })
                                                .unwrap_or_default();
                                            let target_branch = main_wt
                                                .and_then(|w| w.branch)
                                                .or_else(|| current_branch(&target_dir))
                                                .unwrap_or_else(|| "HEAD".to_string());
                                            this.pending_worktree_confirm =
                                                Some(PendingWorktreeConfirm::Merge {
                                                    source_dir: src_dir2,
                                                    branch: actual_src_branch,
                                                    target_dir,
                                                    target_branch,
                                                });
                                            cx.notify();
                                        });
                                    }),
                            );
                        }
                        let v = view.clone();
                        let sid2 = sid.clone();
                        let wt_path = PathBuf::from(&cur_sw.path);
                        menu = menu.item(
                            PopupMenuItem::new(t(lang, "Worktree を削除…", "Remove worktree…"))
                                .on_click(move |_, _, cx| {
                                    let sid3 = sid2.clone();
                                    let p = wt_path.clone();
                                    let dirty = has_uncommitted_changes(&p).unwrap_or(false);
                                    let _ = v.update(cx, |this, cx| {
                                        this.pending_worktree_confirm =
                                            Some(PendingWorktreeConfirm::Remove {
                                                session_id: sid3,
                                                path: p,
                                                is_dirty: dirty,
                                            });
                                        cx.notify();
                                    });
                                }),
                        );
                    }
                }
                menu
            })
            .into_any_element()
    }
}
