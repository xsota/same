use super::*;
use gpui_component::scroll::ScrollableElement;
use same_agent::{
    discover_external_skills, generate_skill_draft, provider_skill_method, GeneratedSkillDraft,
    SkillSynchronizer,
};
use same_core::skills::{
    resolve_skills, validate_skill_md, EffectiveSkillState, ExternalSkill, ImportConflictPolicy,
    ImportPreview, RelatedFileInfo, Skill, SkillAssignment, SkillDeployment, SkillDeploymentStatus,
    SkillManager, SkillOrigin, SkillResolution, SkillScope, SkillSourceType,
};

/// A skill explicitly chosen for the next chat prompt (via the `+` menu or `$` completion).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PromptSkillRef {
    pub(super) name: String,
    pub(super) description: String,
    /// Host path to `SKILL.md`.
    pub(super) skill_md_path: PathBuf,
    /// Short badge label (`Global`, `Project`, `External`).
    pub(super) badge: String,
}

/// Detects a trailing `$query` token in the chat prompt for live skill completion.
/// Returns `(byte_offset_of_dollar, query_slice)` when the prompt ends with `$` or `$partial-name`
/// preceded by start-of-string or whitespace.
pub(super) fn detect_dollar_skill_token(text: &str) -> Option<(usize, &str)> {
    let dollar_pos = text.rfind('$')?;
    // Must be at start of string or immediately after whitespace.
    if dollar_pos > 0 {
        let prev = text[..dollar_pos].chars().next_back()?;
        if !prev.is_whitespace() {
            return None;
        }
    }
    let query = &text[dollar_pos + 1..];
    if query
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        Some((dollar_pos, query))
    } else {
        None
    }
}

/// Extracts all `$skill-name` tokens appearing anywhere in `text` (at start or after whitespace).
pub(super) fn extract_dollar_skill_names(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' {
            let preceded_ok = i == 0
                || text[..i]
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
                if end > start {
                    let name = text[start..end].to_string();
                    if !out.contains(&name) {
                        out.push(name);
                    }
                    i = end;
                    continue;
                }
            }
        }
        i += 1;
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SkillFilterTab {
    All,
    Global,
    Project,
    External,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SelectedSkillItem {
    Creating,
    Managed(String),
    External(String),
}

pub(super) struct SkillsUiState {
    pub(super) manager: SkillManager,
    pub(super) synchronizer: SkillSynchronizer,
    pub(super) filter_tab: SkillFilterTab,
    pub(super) search_input: Entity<InputState>,
    pub(super) skills: Vec<Skill>,
    pub(super) assignments: Vec<SkillAssignment>,
    pub(super) deployments: Vec<SkillDeployment>,
    pub(super) external_skills: Vec<ExternalSkill>,
    pub(super) resolution: SkillResolution,
    pub(super) selected: Option<SelectedSkillItem>,
    pub(super) editor_input: Entity<TextareaState>,
    pub(super) editor_saved: String,
    pub(super) editor_dirty: bool,
    pub(super) preview_mode: bool,
    pub(super) related_files: Vec<RelatedFileInfo>,
    pub(super) external_md_preview: String,
    pub(super) validation_error: Option<String>,
    pub(super) syncing: bool,
    pub(super) discovering: bool,
    pub(super) discovery_generation: u64,

    // New skill form (rendered in the full right-hand pane when selected == Creating)
    pub(super) new_name: Entity<InputState>,
    pub(super) new_desc: Entity<InputState>,
    pub(super) new_body: Entity<TextareaState>,
    pub(super) new_scope: SkillScope,
    pub(super) new_preview_mode: bool,

    // AI-assisted skill creation
    pub(super) ai_prompt_input: Entity<TextareaState>,
    pub(super) ai_provider_id: Option<String>,
    pub(super) ai_generating: bool,
    pub(super) ai_request_id: u64,
    pub(super) ai_error: Option<String>,
    pub(super) pending_generated_draft: Option<GeneratedSkillDraft>,

    // Rename inline input
    pub(super) renaming: bool,
    pub(super) rename_input: Entity<InputState>,

    // Import preview
    pub(super) import_preview: Option<ImportPreview>,
    pub(super) import_scope: SkillScope,
    pub(super) import_policy: ImportConflictPolicy,
}

impl SkillsUiState {
    pub(super) fn new(
        data_dir: PathBuf,
        storage: Arc<Storage>,
        events: EventBus,
        window: &mut Window,
        cx: &mut Context<SameAppView>,
    ) -> Self {
        let manager = SkillManager::new(data_dir.clone(), storage.clone(), events.clone());
        let synchronizer = SkillSynchronizer::new(data_dir, storage, events);

        let search_input = cx.new(|cx| InputState::new(window, cx).placeholder("Filter skills..."));
        let editor_input = cx.new(|cx| TextareaState::new(window, cx).placeholder("SKILL.md"));
        let new_name = cx.new(|cx| InputState::new(window, cx).placeholder("e.g. code-review"));
        let new_desc = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("What this skill does and when the agent should use it")
        });
        let new_body = cx.new(|cx| {
            TextareaState::new(window, cx).placeholder("# Instructions\n\nStep-by-step workflow...")
        });
        let ai_prompt_input = cx.new(|cx| {
            TextareaState::new(window, cx).placeholder(
                "どんなスキルを作りたいか入力（例：PRレビュー時にセキュリティとエラー処理をチェックするスキル）",
            )
        });
        let rename_input = cx.new(|cx| InputState::new(window, cx).placeholder("new-skill-name"));

        Self {
            manager,
            synchronizer,
            filter_tab: SkillFilterTab::All,
            search_input,
            skills: Vec::new(),
            assignments: Vec::new(),
            deployments: Vec::new(),
            external_skills: Vec::new(),
            resolution: SkillResolution::default(),
            selected: None,
            editor_input,
            editor_saved: String::new(),
            editor_dirty: false,
            preview_mode: false,
            related_files: Vec::new(),
            external_md_preview: String::new(),
            validation_error: None,
            syncing: false,
            discovering: false,
            discovery_generation: 0,
            new_name,
            new_desc,
            new_body,
            new_scope: SkillScope::Global,
            new_preview_mode: false,
            ai_prompt_input,
            ai_provider_id: None,
            ai_generating: false,
            ai_request_id: 0,
            ai_error: None,
            pending_generated_draft: None,
            renaming: false,
            rename_input,
            import_preview: None,
            import_scope: SkillScope::Global,
            import_policy: ImportConflictPolicy::Rename,
        }
    }
}

impl SameAppView {
    /// Returns all skills currently callable from the chat composer:
    /// active SAME-managed skills (Global + current Project) plus discovered External skills.
    pub(super) fn available_prompt_skills(&self) -> Vec<PromptSkillRef> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for s in &self.skills_ui.resolution.active_skills {
            if seen.insert(s.name.to_ascii_lowercase()) {
                out.push(PromptSkillRef {
                    name: s.name.clone(),
                    description: s.description.clone(),
                    skill_md_path: PathBuf::from(&s.path).join("SKILL.md"),
                    badge: match s.scope {
                        SkillScope::Global => "Global".to_string(),
                        SkillScope::Project => "Project".to_string(),
                    },
                });
            }
        }
        for ext in &self.skills_ui.external_skills {
            if seen.insert(ext.name.to_ascii_lowercase()) {
                out.push(PromptSkillRef {
                    name: ext.name.clone(),
                    description: ext.description.clone(),
                    skill_md_path: PathBuf::from(&ext.path).join("SKILL.md"),
                    badge: match ext.origin {
                        SkillOrigin::BuiltIn => "Built-in".to_string(),
                        _ => "External".to_string(),
                    },
                });
            }
        }
        out.sort_by(|a, b| {
            let rank = |badge: &str| match badge {
                "Project" => 0u8,
                "Global" => 1u8,
                "External" => 2u8,
                _ => 3u8, // Built-in
            };
            rank(&a.badge)
                .cmp(&rank(&b.badge))
                .then_with(|| a.name.cmp(&b.name))
        });
        out
    }

    pub(super) fn toggle_prompt_skill(&mut self, skill: PromptSkillRef) {
        if let Some(pos) = self
            .selected_prompt_skills
            .iter()
            .position(|s| s.name == skill.name)
        {
            self.selected_prompt_skills.remove(pos);
        } else {
            self.selected_prompt_skills.push(skill);
        }
    }

    /// Searchable skill picker panel shown above the composer when the user
    /// selects "Skills" from the `+` menu.
    pub(super) fn render_skill_picker_popover(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.show_skill_picker {
            return None;
        }
        let theme = cx.theme().clone();
        let lang = self.config.language;
        let query = self
            .skill_picker_search
            .read(cx)
            .value()
            .trim()
            .to_ascii_lowercase();
        let available = self.available_prompt_skills();
        let matches: Vec<PromptSkillRef> = available
            .into_iter()
            .filter(|s| {
                query.is_empty()
                    || s.name.to_ascii_lowercase().contains(&query)
                    || s.description.to_ascii_lowercase().contains(&query)
                    || s.badge.to_ascii_lowercase().contains(&query)
            })
            .collect();

        let rows: Vec<AnyElement> = if matches.is_empty() {
            vec![div()
                .px_3()
                .py_3()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(if query.is_empty() {
                    t(
                        lang,
                        "利用可能なスキルがありません。Skills Manager から作成・インポートできます。",
                        "No active skills available. Create or import one in Skills Manager.",
                    )
                } else {
                    t(lang, "一致するスキルがありません", "No matching skills")
                })
                .into_any_element()]
        } else {
            matches
                .into_iter()
                .enumerate()
                .map(|(i, skill)| {
                    let selected = self
                        .selected_prompt_skills
                        .iter()
                        .any(|s| s.name == skill.name);
                    let sk_click = skill.clone();
                    list_row(
                        SharedString::from(format!("skill-picker-row-{i}")),
                        selected,
                        &theme,
                    )
                    .gap_2()
                    .child(
                        div()
                            .w(px(14.0))
                            .flex_shrink_0()
                            .text_xs()
                            .text_color(accent_text())
                            .child(if selected { "✓" } else { "" }),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(if selected {
                                accent_text().into()
                            } else {
                                theme.foreground
                            })
                            .child(format!("${}", skill.name)),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_xs()
                            .px_1()
                            .rounded(px(3.0))
                            .bg(theme.secondary)
                            .text_color(theme.muted_foreground)
                            .child(skill.badge.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(skill.description.clone()),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_prompt_skill(sk_click.clone());
                        cx.notify();
                    }))
                    .into_any_element()
                })
                .collect()
        };

        Some(
            div()
                .id("composer-skill-picker")
                .v_flex()
                .w_full()
                .p_2()
                .gap_1p5()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .bg(theme.sidebar)
                .shadow_md()
                .child(
                    div()
                        .h_flex()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .child(
                            div()
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(accent_text())
                                .child(t(lang, "✦ スキルを選択", "✦ Select Skills")),
                        )
                        .child(
                            div()
                                .h_flex()
                                .items_center()
                                .gap_1()
                                .child(
                                    Button::new("btn-skill-picker-manage")
                                        .xsmall()
                                        .ghost()
                                        .label(t(lang, "管理画面", "Manage"))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.show_skill_picker = false;
                                            this.open_skills_view();
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    Button::new("btn-skill-picker-close")
                                        .xsmall()
                                        .ghost()
                                        .label(t(lang, "完了", "Done"))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.show_skill_picker = false;
                                            this.prompt_input
                                                .update(cx, |ta, cx| ta.focus(window, cx));
                                            cx.notify();
                                        })),
                                ),
                        ),
                )
                .child(Input::new(&self.skill_picker_search).small())
                .child(
                    div()
                        .id("composer-skill-picker-list")
                        .v_flex()
                        .w_full()
                        .max_h(px(220.0))
                        .overflow_y_scroll()
                        .children(rows),
                )
                .into_any_element(),
        )
    }

    /// Suggestions popup shown above the composer when typing `$skill` anywhere in the prompt.
    pub(super) fn render_skill_suggestions(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.prompt_is_skill_mention {
            return None;
        }
        let text = self.prompt_input.read(cx).value().to_string();
        let (dollar_pos, query) = detect_dollar_skill_token(&text)?;
        let q = query.to_ascii_lowercase();
        let available = self.available_prompt_skills();
        let matches: Vec<PromptSkillRef> = available
            .into_iter()
            .filter(|s| {
                q.is_empty()
                    || s.name.to_ascii_lowercase().contains(&q)
                    || s.description.to_ascii_lowercase().contains(&q)
            })
            .take(30)
            .collect();
        if matches.is_empty() {
            return None;
        }
        let theme = cx.theme().clone();
        let prefix = text[..dollar_pos].to_string();

        Some(
            div()
                .id("skill-suggestions")
                .v_flex()
                .w_full()
                .max_h(px(220.0))
                .overflow_y_scroll()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .bg(theme.sidebar)
                .children(matches.into_iter().enumerate().map(|(i, skill)| {
                    let selected = self
                        .selected_prompt_skills
                        .iter()
                        .any(|s| s.name == skill.name);
                    let fill = format!("{prefix}${} ", skill.name);
                    list_row(
                        SharedString::from(format!("skill-sug-{i}")),
                        selected,
                        &theme,
                    )
                    .gap_2()
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(accent_text())
                            .child(format!("${}", skill.name)),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_xs()
                            .px_1()
                            .rounded(px(3.0))
                            .bg(theme.secondary)
                            .text_color(theme.muted_foreground)
                            .child(skill.badge.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(skill.description.clone()),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.prompt_input.update(cx, |ta, cx| {
                            ta.set_value(fill.clone(), window, cx);
                            ta.focus(window, cx);
                        });
                        this.prompt_empty = false;
                        this.prompt_is_skill_mention = false;
                        cx.notify();
                    }))
                }))
                .into_any_element(),
        )
    }

    /// Reloads skills, assignments, deployments, and recomputes resolution for the
    /// currently selected project (refreshing hashes from disk).
    pub(super) fn reload_skills_state(&mut self) {
        self.skills_ui.skills = self
            .skills_ui
            .manager
            .refresh_from_disk()
            .unwrap_or_else(|_| self.skills_ui.manager.list_skills().unwrap_or_default());
        self.reload_skills_project_bindings();
    }

    /// Lightweight reload from SQLite without re-hashing every skill directory on disk.
    pub(super) fn reload_skills_state_fast(&mut self) {
        self.skills_ui.skills = self.skills_ui.manager.list_skills().unwrap_or_default();
        self.reload_skills_project_bindings();
    }

    fn reload_skills_project_bindings(&mut self) {
        self.skills_ui.assignments = match self.selected_project_id.as_deref() {
            Some(pid) => self
                .skills_ui
                .manager
                .list_assignments(pid)
                .unwrap_or_default(),
            None => Vec::new(),
        };
        self.skills_ui.deployments = self
            .storage
            .list_skill_deployments(None)
            .unwrap_or_default();
        self.recompute_skill_resolution();
    }

    pub(super) fn recompute_skill_resolution(&mut self) {
        self.skills_ui.resolution = resolve_skills(
            self.selected_project_id.as_deref(),
            &self.skills_ui.skills,
            &self.skills_ui.assignments,
            &self.skills_ui.external_skills,
        );
    }

    pub(super) fn trigger_external_skill_discovery(&mut self) {
        self.skills_ui.discovery_generation = self.skills_ui.discovery_generation.wrapping_add(1);
        let generation = self.skills_ui.discovery_generation;
        self.skills_ui.discovering = true;
        // Clear stale project-scoped external skills immediately on context change.
        self.skills_ui.external_skills.clear();
        self.recompute_skill_resolution();
        let exec = self.exec_pref();
        let cwd = self.current_project().map(|p| PathBuf::from(&p.workdir));
        let tx = self.ui_tx.clone();
        self.rt.spawn(async move {
            let probe_cwd = cwd.as_deref().unwrap_or_else(|| Path::new("."));
            let target = agent_exec::resolve_target(exec, probe_cwd).await;
            let skills = discover_external_skills(&target, cwd.as_deref()).await;
            let _ = tx.send(UiAsyncMsg::ExternalSkillsDiscovered { generation, skills });
        });
    }

    pub(super) fn trigger_skills_sync_all(&mut self, manual: bool) {
        if self.skills_ui.syncing {
            return;
        }
        self.skills_ui.syncing = true;
        let synchronizer = self.skills_ui.synchronizer.clone();
        let providers: Vec<ProviderConfig> = self
            .config
            .providers
            .iter()
            .filter(|p| p.enabled)
            .cloned()
            .collect();
        let project_id = self.selected_project_id.clone();
        let resolution = self.skills_ui.resolution.clone();
        let exec = self.exec_pref();
        let cwd = self
            .current_project()
            .map(|p| PathBuf::from(&p.workdir))
            .unwrap_or_else(|| PathBuf::from("."));
        let tx = self.ui_tx.clone();
        self.rt.spawn(async move {
            let target = agent_exec::resolve_target(exec, &cwd).await;
            let mut synced_providers = 0usize;
            let mut errors = Vec::new();
            for provider in &providers {
                match synchronizer.sync_for_provider(
                    provider,
                    project_id.as_deref(),
                    &target,
                    &resolution,
                ) {
                    Ok(_) => synced_providers += 1,
                    Err(e) => errors.push(format!("{}: {e:#}", provider.name)),
                }
            }
            let result = if errors.is_empty() {
                Ok(synced_providers)
            } else {
                Err(errors.join("; "))
            };
            let _ = tx.send(UiAsyncMsg::SkillsSynced { result, manual });
        });
    }

    pub(super) fn open_skills_view(&mut self) {
        self.active_view = MainView::Skills;
        self.reload_skills_state();
        self.trigger_external_skill_discovery();
    }

    pub(super) fn select_managed_skill(
        &mut self,
        skill_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let content = self
            .skills_ui
            .manager
            .read_skill_md(&skill_id)
            .unwrap_or_default();
        self.skills_ui.related_files = self
            .skills_ui
            .manager
            .list_related_files(&skill_id)
            .unwrap_or_default();
        self.skills_ui.editor_saved = content.clone();
        self.skills_ui.editor_dirty = false;
        self.skills_ui.validation_error = None;
        self.skills_ui.renaming = false;
        self.skills_ui.selected = Some(SelectedSkillItem::Managed(skill_id));
        self.skills_ui.editor_input.update(cx, |s, cx| {
            s.set_value(content, window, cx);
        });
    }

    pub(super) fn select_external_skill(&mut self, path: String) {
        let md_path = Path::new(&path).join("SKILL.md");
        self.skills_ui.external_md_preview = std::fs::read_to_string(&md_path).unwrap_or_default();
        self.skills_ui.selected = Some(SelectedSkillItem::External(path));
        self.skills_ui.editor_dirty = false;
        self.skills_ui.renaming = false;
    }

    pub(super) fn save_selected_skill(&mut self, cx: &mut Context<Self>) {
        let lang = self.config.language;
        let Some(SelectedSkillItem::Managed(skill_id)) = self.skills_ui.selected.clone() else {
            return;
        };
        let content = self.skills_ui.editor_input.read(cx).value().to_string();
        let (_, report) = validate_skill_md(&content, None);
        if !report.is_valid() {
            let msg = report.error_summary();
            self.skills_ui.validation_error = Some(msg.clone());
            self.pending_notes.push((NoteKind::Error, msg));
            return;
        }
        match self.skills_ui.manager.update_skill_md(&skill_id, &content) {
            Ok(_) => {
                self.skills_ui.editor_saved = content;
                self.skills_ui.editor_dirty = false;
                self.skills_ui.validation_error = None;
                self.reload_skills_state();
                self.trigger_skills_sync_all(false);
                self.pending_notes.push((
                    NoteKind::Success,
                    t(lang, "スキルを保存しました", "Skill saved").to_string(),
                ));
            }
            Err(err) => {
                let msg = format!("{err:#}");
                self.skills_ui.validation_error = Some(msg.clone());
                self.pending_notes.push((NoteKind::Error, msg));
            }
        }
    }

    fn create_skill_from_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let lang = self.config.language;
        let name = self.skills_ui.new_name.read(cx).value().trim().to_string();
        let desc = self.skills_ui.new_desc.read(cx).value().trim().to_string();
        let body = self.skills_ui.new_body.read(cx).value().to_string();
        let scope = self.skills_ui.new_scope;
        let project_id = match scope {
            SkillScope::Global => None,
            SkillScope::Project => self.selected_project_id.clone(),
        };
        match self
            .skills_ui
            .manager
            .create_skill(&name, &desc, scope, project_id.as_deref(), &body)
        {
            Ok(skill) => {
                self.skills_ui.ai_error = None;
                self.skills_ui.new_preview_mode = false;
                self.skills_ui
                    .new_name
                    .update(cx, |s, cx| s.set_value("", window, cx));
                self.skills_ui
                    .new_desc
                    .update(cx, |s, cx| s.set_value("", window, cx));
                self.skills_ui
                    .new_body
                    .update(cx, |s, cx| s.set_value("", window, cx));
                self.skills_ui
                    .ai_prompt_input
                    .update(cx, |s, cx| s.set_value("", window, cx));
                self.reload_skills_state();
                self.select_managed_skill(skill.id, window, cx);
                self.trigger_skills_sync_all(false);
                self.pending_notes.push((
                    NoteKind::Success,
                    t(lang, "スキルを作成しました", "Skill created").to_string(),
                ));
            }
            Err(err) => {
                self.pending_notes
                    .push((NoteKind::Error, format!("{err:#}")));
            }
        }
    }

    pub(super) fn apply_pending_skill_draft(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(draft) = self.skills_ui.pending_generated_draft.take() {
            self.skills_ui.selected = Some(SelectedSkillItem::Creating);
            self.skills_ui
                .new_name
                .update(cx, |s, cx| s.set_value(draft.name, window, cx));
            self.skills_ui
                .new_desc
                .update(cx, |s, cx| s.set_value(draft.description, window, cx));
            self.skills_ui
                .new_body
                .update(cx, |s, cx| s.set_value(draft.body, window, cx));
        }
    }

    fn selected_ai_provider(&self) -> Option<ProviderConfig> {
        let is_selectable = |p: &&ProviderConfig| {
            self.is_provider_visible_in_workspace(p)
                && !self
                    .provider_quota_summary(&p.id)
                    .is_some_and(|q| q.exhausted)
        };
        if let Some(explicit) = self.skills_ui.ai_provider_id.as_deref() {
            return self
                .config
                .providers
                .iter()
                .find(|p| p.id == explicit && is_selectable(p))
                .cloned();
        }
        let default_id = &self.config.default_provider_id;
        self.config
            .providers
            .iter()
            .find(|p| p.id == *default_id && is_selectable(p))
            .or_else(|| self.config.providers.iter().find(is_selectable))
            .cloned()
    }

    fn trigger_ai_skill_generation(&mut self, cx: &mut Context<Self>) {
        let lang = self.config.language;
        let request = self
            .skills_ui
            .ai_prompt_input
            .read(cx)
            .value()
            .trim()
            .to_string();
        if request.is_empty() {
            self.pending_notes.push((
                NoteKind::Warning,
                t(
                    lang,
                    "AIに作成してほしいスキルの内容を入力してください",
                    "Describe the skill you want AI to draft",
                )
                .to_string(),
            ));
            return;
        }
        let Some(provider) = self.selected_ai_provider() else {
            self.pending_notes.push((
                NoteKind::Error,
                t(
                    lang,
                    "利用可能なAIプロバイダが設定されていません",
                    "No enabled AI provider is configured",
                )
                .to_string(),
            ));
            return;
        };

        let existing_name = self.skills_ui.new_name.read(cx).value().to_string();
        let existing_desc = self.skills_ui.new_desc.read(cx).value().to_string();
        let existing_body = self.skills_ui.new_body.read(cx).value().to_string();
        let scope = self.skills_ui.new_scope;
        let exec = self.exec_pref();
        let cwd = self
            .current_project()
            .map(|p| PathBuf::from(&p.workdir))
            .unwrap_or_else(|| PathBuf::from("."));

        self.skills_ui.ai_request_id = self.skills_ui.ai_request_id.wrapping_add(1);
        let request_id = self.skills_ui.ai_request_id;
        self.skills_ui.ai_generating = true;
        self.skills_ui.ai_error = None;
        let tx = self.ui_tx.clone();

        self.rt.spawn(async move {
            let target = agent_exec::resolve_target(exec, &cwd).await;
            let existing = if existing_name.trim().is_empty()
                && existing_desc.trim().is_empty()
                && existing_body.trim().is_empty()
            {
                None
            } else {
                Some((
                    existing_name.as_str(),
                    existing_desc.as_str(),
                    existing_body.as_str(),
                ))
            };
            let result = generate_skill_draft(&provider, &target, &cwd, &request, scope, existing)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(UiAsyncMsg::SkillDraftGenerated { request_id, result });
        });
    }

    fn pick_local_import(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: true,
            multiple: false,
            prompt: Some(SharedString::from(t(
                self.config.language,
                "インポートするスキルフォルダまたは SKILL.md を選択",
                "Select a skill folder or SKILL.md to import",
            ))),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                if let Some(src) = paths.into_iter().next() {
                    let _ = this.update(cx, |this, cx| {
                        let scope = this.skills_ui.import_scope;
                        let pid = match scope {
                            SkillScope::Global => None,
                            SkillScope::Project => this.selected_project_id.as_deref(),
                        };
                        match this
                            .skills_ui
                            .manager
                            .inspect_import_source(&src, scope, pid)
                        {
                            Ok(preview) => {
                                if preview.candidates.is_empty() {
                                    this.pending_notes.push((
                                        NoteKind::Warning,
                                        t(
                                            this.config.language,
                                            "SKILL.md が見つかりませんでした",
                                            "No SKILL.md found in selected path",
                                        )
                                        .to_string(),
                                    ));
                                } else {
                                    this.skills_ui.import_preview = Some(preview);
                                }
                            }
                            Err(e) => {
                                this.pending_notes.push((NoteKind::Error, format!("{e:#}")));
                            }
                        }
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    fn confirm_import_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let lang = self.config.language;
        let Some(preview) = self.skills_ui.import_preview.take() else {
            return;
        };
        let scope = self.skills_ui.import_scope;
        let pid = match scope {
            SkillScope::Global => None,
            SkillScope::Project => self.selected_project_id.clone(),
        };
        let valid_candidates: Vec<_> = preview
            .candidates
            .into_iter()
            .filter(|c| c.validation.is_valid())
            .collect();
        match self.skills_ui.manager.import_candidates(
            &valid_candidates,
            scope,
            pid.as_deref(),
            self.skills_ui.import_policy,
            SkillSourceType::Local,
        ) {
            Ok(imported) => {
                let count = imported.len();
                self.reload_skills_state();
                if let Some(first) = imported.first() {
                    self.select_managed_skill(first.id.clone(), window, cx);
                }
                self.trigger_skills_sync_all(false);
                self.pending_notes.push((
                    NoteKind::Success,
                    match lang {
                        Language::Ja => format!("{count} 件のスキルをインポートしました"),
                        Language::En => format!("Imported {count} skill(s)"),
                    },
                ));
            }
            Err(err) => {
                self.pending_notes
                    .push((NoteKind::Error, format!("{err:#}")));
            }
        }
    }

    fn export_selected_skill(&mut self, cx: &mut Context<Self>) {
        let Some(SelectedSkillItem::Managed(skill_id)) = self.skills_ui.selected.clone() else {
            return;
        };
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(SharedString::from(t(
                self.config.language,
                "エクスポート先フォルダを選択",
                "Select export destination folder",
            ))),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                if let Some(dest) = paths.into_iter().next() {
                    let _ = this.update(cx, |this, cx| {
                        match this.skills_ui.manager.export_skill(&skill_id, &dest) {
                            Ok(out_path) => {
                                this.pending_notes.push((
                                    NoteKind::Success,
                                    match this.config.language {
                                        Language::Ja => {
                                            format!("エクスポートしました: {}", out_path.display())
                                        }
                                        Language::En => {
                                            format!("Exported to {}", out_path.display())
                                        }
                                    },
                                ));
                            }
                            Err(e) => {
                                this.pending_notes.push((NoteKind::Error, format!("{e:#}")));
                            }
                        }
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    fn add_related_file_to_selected(&mut self, cx: &mut Context<Self>) {
        let Some(SelectedSkillItem::Managed(skill_id)) = self.skills_ui.selected.clone() else {
            return;
        };
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(SharedString::from(t(
                self.config.language,
                "スキルに追加する関連ファイルを選択",
                "Select a supporting file to add to this skill",
            ))),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                if let Some(src) = paths.into_iter().next() {
                    let _ = this.update(cx, |this, cx| {
                        let fname = src
                            .file_name()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_else(|| "file".to_string());
                        let is_script = src
                            .extension()
                            .and_then(|e| e.to_str())
                            .map(|e| {
                                matches!(
                                    e.to_ascii_lowercase().as_str(),
                                    "sh" | "ps1" | "py" | "js" | "ts" | "rb" | "bat" | "cmd"
                                )
                            })
                            .unwrap_or(false);
                        let rel = if is_script {
                            format!("scripts/{fname}")
                        } else {
                            format!("references/{fname}")
                        };
                        match this
                            .skills_ui
                            .manager
                            .add_related_file(&skill_id, &rel, &src)
                        {
                            Ok(_) => {
                                this.skills_ui.related_files = this
                                    .skills_ui
                                    .manager
                                    .list_related_files(&skill_id)
                                    .unwrap_or_default();
                                this.reload_skills_state();
                                this.trigger_skills_sync_all(false);
                            }
                            Err(e) => {
                                this.pending_notes.push((NoteKind::Error, format!("{e:#}")));
                            }
                        }
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    fn import_external_skill(
        &mut self,
        ext_path: String,
        scope: SkillScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let lang = self.config.language;
        let pid = match scope {
            SkillScope::Global => None,
            SkillScope::Project => self.selected_project_id.clone(),
        };
        match self.skills_ui.manager.inspect_import_source(
            Path::new(&ext_path),
            scope,
            pid.as_deref(),
        ) {
            Ok(preview) => {
                match self.skills_ui.manager.import_candidates(
                    &preview.candidates,
                    scope,
                    pid.as_deref(),
                    ImportConflictPolicy::Rename,
                    SkillSourceType::External,
                ) {
                    Ok(imported) => {
                        self.reload_skills_state();
                        if let Some(first) = imported.first() {
                            self.select_managed_skill(first.id.clone(), window, cx);
                        }
                        self.trigger_skills_sync_all(false);
                        self.pending_notes.push((
                            NoteKind::Success,
                            t(
                                lang,
                                "外部スキルを SAME にインポートしました",
                                "Imported external skill into SAME",
                            )
                            .to_string(),
                        ));
                    }
                    Err(e) => {
                        self.pending_notes.push((NoteKind::Error, format!("{e:#}")));
                    }
                }
            }
            Err(e) => {
                self.pending_notes.push((NoteKind::Error, format!("{e:#}")));
            }
        }
    }

    pub(super) fn render_skills_view(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let lang = self.config.language;
        let query = self
            .skills_ui
            .search_input
            .read(cx)
            .value()
            .trim()
            .to_ascii_lowercase();
        let active_count = self.skills_ui.resolution.active_skills.len();
        let conflict_count = self.skills_ui.resolution.conflicts.len();
        let project_name = self
            .current_project()
            .map(|p| p.name.clone())
            .unwrap_or_else(|| t(lang, "プロジェクト未選択", "No Project").to_string());

        // Filter managed entries
        let managed_entries: Vec<_> = self
            .skills_ui
            .resolution
            .entries
            .iter()
            .filter(|e| match self.skills_ui.filter_tab {
                SkillFilterTab::All => true,
                SkillFilterTab::Global => e.skill.scope == SkillScope::Global,
                SkillFilterTab::Project => e.skill.scope == SkillScope::Project,
                SkillFilterTab::External => false,
            })
            .filter(|e| {
                query.is_empty()
                    || e.skill.name.to_ascii_lowercase().contains(&query)
                    || e.skill.description.to_ascii_lowercase().contains(&query)
            })
            .cloned()
            .collect();

        let mut external_items: Vec<_> = self
            .skills_ui
            .external_skills
            .iter()
            .filter(|e| match self.skills_ui.filter_tab {
                SkillFilterTab::All | SkillFilterTab::External => true,
                SkillFilterTab::Global => false,
                // Project tab includes only repo-local external skills (e.g. `.agents/skills`),
                // excluding machine-wide `~/.claude/skills` and built-in skills.
                SkillFilterTab::Project => {
                    e.origin != SkillOrigin::BuiltIn && !e.source_label.starts_with('~')
                }
            })
            .filter(|e| {
                query.is_empty()
                    || e.name.to_ascii_lowercase().contains(&query)
                    || e.description.to_ascii_lowercase().contains(&query)
                    || e.source_label.to_ascii_lowercase().contains(&query)
            })
            .cloned()
            .collect();
        external_items.sort_by(|a, b| {
            let rank = |origin: SkillOrigin| match origin {
                SkillOrigin::External => 0u8,
                SkillOrigin::BuiltIn => 1u8,
                _ => 2u8,
            };
            rank(a.origin)
                .cmp(&rank(b.origin))
                .then_with(|| a.name.cmp(&b.name))
        });

        div()
            .v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(theme.background)
            // Top bar
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .justify_between()
                    .px_4()
                    .py_2p5()
                    .gap_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .h_flex()
                            .items_center()
                            .gap_2()
                            .child(div().text_base().font_weight(FontWeight::BOLD).child(t(
                                lang,
                                "Skills Manager",
                                "Skills Manager",
                            )))
                            .child(
                                div()
                                    .text_xs()
                                    .px_1p5()
                                    .py(px(2.0))
                                    .rounded(px(4.0))
                                    .bg(theme.secondary)
                                    .text_color(theme.secondary_foreground)
                                    .child(format!(
                                        "{project_name} · {}",
                                        match lang {
                                            Language::Ja => format!("有効 {active_count} 件"),
                                            Language::En => format!("{active_count} active"),
                                        }
                                    )),
                            )
                            .when(conflict_count > 0, |el| {
                                el.child(
                                    div()
                                        .text_xs()
                                        .px_1p5()
                                        .py(px(2.0))
                                        .rounded(px(4.0))
                                        .bg(status_amber().opacity(0.18))
                                        .text_color(status_amber())
                                        .child(match lang {
                                            Language::Ja => format!("衝突 {conflict_count} 件"),
                                            Language::En => format!("{conflict_count} conflict(s)"),
                                        }),
                                )
                            }),
                    )
                    .child(
                        div()
                            .h_flex()
                            .items_center()
                            .gap_1p5()
                            .child(
                                Button::new("btn-skill-new")
                                    .xsmall()
                                    .primary()
                                    .label(t(lang, "+ 新規作成", "+ New Skill"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.skills_ui.selected = Some(SelectedSkillItem::Creating);
                                        this.skills_ui.ai_error = None;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("btn-skill-import")
                                    .xsmall()
                                    .outline()
                                    .label(t(lang, "インポート", "Import"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.pick_local_import(cx);
                                    })),
                            )
                            .child(
                                Button::new("btn-skill-sync")
                                    .xsmall()
                                    .outline()
                                    .disabled(self.skills_ui.syncing)
                                    .label(if self.skills_ui.syncing {
                                        t(lang, "同期中...", "Syncing...")
                                    } else {
                                        t(lang, "同期", "Sync")
                                    })
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.reload_skills_state();
                                        this.trigger_skills_sync_all(true);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("btn-skill-refresh")
                                    .xsmall()
                                    .ghost()
                                    .label(t(lang, "再読込", "Refresh"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.reload_skills_state();
                                        this.trigger_external_skill_discovery();
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            // Optional import confirmation panel
            .when(self.skills_ui.import_preview.is_some(), |el| {
                el.child(self.render_import_preview_panel(cx))
            })
            // Main split body: Left list + Right detail/editor
            .child(
                div()
                    .h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_skills_list_pane(&managed_entries, &external_items, cx))
                    .child(self.render_skill_detail_pane(cx)),
            )
    }

    #[inline(never)]
    fn render_create_skill_pane(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .p_5()
            .gap_4()
            .overflow_y_scrollbar()
            .child(self.render_create_skill_header(cx))
            .child(self.render_create_skill_ai_card(cx))
            .child(self.render_create_skill_name_scope(cx))
            .child(self.render_create_skill_description())
            .child(self.render_create_skill_instructions(cx))
            .child(self.render_create_skill_footer(cx))
            .into_any_element()
    }

    #[inline(never)]
    fn render_create_skill_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let lang = self.config.language;
        div()
            .h_flex()
            .items_center()
            .justify_between()
            .gap_2()
            .child(
                div()
                    .v_flex()
                    .gap_0p5()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::BOLD)
                            .child(t(lang, "新規スキル作成", "Create New Skill")),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(t(
                                lang,
                                "AIに下書きを依頼することも、最初から手動で編集することもできます（Agent Skills 標準形式）",
                                "Draft with AI or write manually from scratch (Agent Skills standard format)",
                            )),
                    ),
            )
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_1p5()
                    .child(
                        Button::new("btn-create-skill-cancel")
                            .xsmall()
                            .ghost()
                            .label(t(lang, "キャンセル", "Cancel"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.skills_ui.selected = None;
                                this.skills_ui.ai_generating = false;
                                this.skills_ui.ai_request_id =
                                    this.skills_ui.ai_request_id.wrapping_add(1);
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("btn-create-skill-submit-top")
                            .xsmall()
                            .primary()
                            .label(t(lang, "スキルを作成", "Create Skill"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.create_skill_from_form(window, cx);
                                cx.notify();
                            })),
                    ),
            )
            .into_any_element()
    }

    #[inline(never)]
    fn render_create_skill_ai_card(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let lang = self.config.language;
        let enabled_providers: Vec<ProviderConfig> = self
            .config
            .providers
            .iter()
            .filter(|p| self.is_provider_visible_in_workspace(p))
            .cloned()
            .collect();
        let active_provider_id = self
            .selected_ai_provider()
            .map(|p| p.id)
            .unwrap_or_default();

        let provider_buttons: Vec<AnyElement> = enabled_providers
            .iter()
            .map(|p| {
                let pid = p.id.clone();
                let exhausted_suffix = self
                    .provider_quota_summary(&p.id)
                    .filter(|q| q.exhausted)
                    .map(|q| Self::format_exhausted_quota_suffix(&q, lang));
                let exhausted = exhausted_suffix.is_some();
                let selected = !exhausted && p.id == active_provider_id;
                let label = match exhausted_suffix {
                    Some(suffix) => format!("{} {suffix}", p.name),
                    None => p.name.clone(),
                };
                Button::new(SharedString::from(format!("ai-prov-{}", p.id)))
                    .xsmall()
                    .disabled(exhausted)
                    .when(selected, |b| b.primary())
                    .when(!selected, |b| b.ghost())
                    .label(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.skills_ui.ai_provider_id = Some(pid.clone());
                        cx.notify();
                    }))
                    .into_any_element()
            })
            .collect();

        let mut card = div()
            .v_flex()
            .p_3()
            .gap_2p5()
            .rounded(px(8.0))
            .bg(theme.sidebar)
            .border_1()
            .border_color(accent().opacity(0.45))
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(accent_text())
                            .child(t(lang, "✨ AIにお願いして下書きを作る", "✨ Draft with AI")),
                    )
                    .child(
                        div()
                            .h_flex()
                            .items_center()
                            .gap_1()
                            .children(provider_buttons),
                    ),
            )
            .child(
                div()
                    .h(px(88.0))
                    .child(Textarea::new(&self.skills_ui.ai_prompt_input).h_full()),
            );

        if let Some(err) = self.skills_ui.ai_error.clone() {
            card = card.child(
                div()
                    .text_xs()
                    .p_2()
                    .rounded(px(4.0))
                    .bg(status_red().opacity(0.15))
                    .text_color(status_red())
                    .child(err),
            );
        }

        let mut actions = div().h_flex().items_center().gap_1p5();
        if self.skills_ui.ai_generating {
            actions = actions.child(
                Button::new("btn-ai-gen-cancel")
                    .xsmall()
                    .ghost()
                    .label(t(lang, "中断", "Stop"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.skills_ui.ai_generating = false;
                        this.skills_ui.ai_request_id = this.skills_ui.ai_request_id.wrapping_add(1);
                        cx.notify();
                    })),
            );
        }
        actions = actions.child(
            Button::new("btn-ai-gen-run")
                .xsmall()
                .primary()
                .disabled(self.skills_ui.ai_generating || enabled_providers.is_empty())
                .label(if self.skills_ui.ai_generating {
                    t(lang, "AIが生成中...", "Generating...")
                } else {
                    t(lang, "✨ AIで下書きを生成", "✨ Generate Draft")
                })
                .on_click(cx.listener(|this, _, _, cx| {
                    this.trigger_ai_skill_generation(cx);
                    cx.notify();
                })),
        );

        card.child(
            div()
                .h_flex()
                .items_center()
                .justify_between()
                .gap_2()
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(t(
                            lang,
                            "生成結果は下のフォームに反映され、作成前に自由に編集できます。",
                            "Generated content fills the form below so you can freely edit before creating.",
                        )),
                )
                .child(actions),
        )
        .into_any_element()
    }

    #[inline(never)]
    fn render_create_skill_name_scope(&self, cx: &mut Context<Self>) -> AnyElement {
        let lang = self.config.language;
        let has_project = self.selected_project_id.is_some();
        div()
            .h_flex()
            .gap_3()
            .items_end()
            .child(
                div()
                    .v_flex()
                    .gap_1()
                    .flex_1()
                    .child(div().text_xs().font_weight(FontWeight::MEDIUM).child(t(
                        lang,
                        "スキル名（小文字英数字とハイフン 例: pr-security-review）",
                        "Skill Name (lowercase letters, digits, hyphens, e.g. pr-security-review)",
                    )))
                    .child(Input::new(&self.skills_ui.new_name)),
            )
            .child(
                div()
                    .v_flex()
                    .gap_1()
                    .flex_shrink_0()
                    .child(div().text_xs().font_weight(FontWeight::MEDIUM).child(t(
                        lang,
                        "管理範囲",
                        "Scope",
                    )))
                    .child(
                        div()
                            .h_flex()
                            .gap_1()
                            .child(
                                Button::new("new-scope-global")
                                    .small()
                                    .when(self.skills_ui.new_scope == SkillScope::Global, |b| {
                                        b.primary()
                                    })
                                    .when(self.skills_ui.new_scope != SkillScope::Global, |b| {
                                        b.outline()
                                    })
                                    .label("Global")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.skills_ui.new_scope = SkillScope::Global;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("new-scope-project")
                                    .small()
                                    .disabled(!has_project)
                                    .when(self.skills_ui.new_scope == SkillScope::Project, |b| {
                                        b.primary()
                                    })
                                    .when(self.skills_ui.new_scope != SkillScope::Project, |b| {
                                        b.outline()
                                    })
                                    .label("Project")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.skills_ui.new_scope = SkillScope::Project;
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }

    #[inline(never)]
    fn render_create_skill_description(&self) -> AnyElement {
        let lang = self.config.language;
        div()
            .v_flex()
            .gap_1()
            .child(div().text_xs().font_weight(FontWeight::MEDIUM).child(t(
                lang,
                "説明（AIがこのスキルを使うべき条件を判断するための説明）",
                "Description (tells agents what this skill does and when to activate it)",
            )))
            .child(Input::new(&self.skills_ui.new_desc))
            .into_any_element()
    }

    #[inline(never)]
    fn render_create_skill_instructions(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let lang = self.config.language;
        let editor_box = if self.skills_ui.new_preview_mode {
            let draft_body = self.skills_ui.new_body.read(cx).value().to_string();
            div()
                .p_3()
                .size_full()
                .overflow_y_scrollbar()
                .child(TextView::markdown("new-skill-md-preview", draft_body).selectable(true))
                .into_any_element()
        } else {
            Textarea::new(&self.skills_ui.new_body)
                .h_full()
                .into_any_element()
        };

        div()
            .v_flex()
            .gap_1p5()
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_xs().font_weight(FontWeight::MEDIUM).child(t(
                        lang,
                        "指示内容 (SKILL.md 本文)",
                        "Instructions (SKILL.md body)",
                    )))
                    .child(
                        Button::new("btn-new-preview-toggle")
                            .xsmall()
                            .outline()
                            .label(if self.skills_ui.new_preview_mode {
                                t(lang, "編集に戻る", "Back to Edit")
                            } else {
                                t(lang, "Markdown プレビュー", "Markdown Preview")
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.skills_ui.new_preview_mode = !this.skills_ui.new_preview_mode;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .h(px(360.0))
                    .border_1()
                    .border_color(theme.border)
                    .rounded(px(6.0))
                    .overflow_hidden()
                    .child(editor_box),
            )
            .into_any_element()
    }

    #[inline(never)]
    fn render_create_skill_footer(&self, cx: &mut Context<Self>) -> AnyElement {
        let lang = self.config.language;
        div()
            .h_flex()
            .justify_end()
            .gap_2()
            .child(
                Button::new("btn-create-skill-submit")
                    .small()
                    .primary()
                    .label(t(lang, "スキルを作成", "Create Skill"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.create_skill_from_form(window, cx);
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    fn render_import_preview_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let lang = self.config.language;
        let Some(preview) = &self.skills_ui.import_preview else {
            return div().into_any_element();
        };
        let has_project = self.selected_project_id.is_some();
        let any_scripts = preview.candidates.iter().any(|c| c.has_executable_scripts);
        let any_collision = preview.candidates.iter().any(|c| c.collides_with_existing);

        div()
            .v_flex()
            .p_3()
            .gap_2()
            .bg(theme.sidebar)
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(match lang {
                                Language::Ja => format!(
                                    "インポート確認 ({} 件検出): {}",
                                    preview.candidates.len(),
                                    preview.source_root.display()
                                ),
                                Language::En => format!(
                                    "Import Preview ({} found): {}",
                                    preview.candidates.len(),
                                    preview.source_root.display()
                                ),
                            }),
                    )
                    .child(
                        div()
                            .h_flex()
                            .gap_1()
                            .child(
                                Button::new("imp-scope-global")
                                    .xsmall()
                                    .when(self.skills_ui.import_scope == SkillScope::Global, |b| b.primary())
                                    .when(self.skills_ui.import_scope != SkillScope::Global, |b| b.ghost())
                                    .label("Global")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.skills_ui.import_scope = SkillScope::Global;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("imp-scope-project")
                                    .xsmall()
                                    .disabled(!has_project)
                                    .when(self.skills_ui.import_scope == SkillScope::Project, |b| b.primary())
                                    .when(self.skills_ui.import_scope != SkillScope::Project, |b| b.ghost())
                                    .label("Project")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.skills_ui.import_scope = SkillScope::Project;
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .when(any_scripts, |el| {
                el.child(
                    div()
                        .text_xs()
                        .px_2()
                        .py_1()
                        .rounded(px(4.0))
                        .bg(status_amber().opacity(0.16))
                        .text_color(status_amber())
                        .child(t(
                            lang,
                            "⚠ 実行可能なスクリプトが含まれています。インポート時に自動実行はされませんが、内容を確認してください。",
                            "⚠ Contains executable scripts. Scripts are never auto-executed on import, but please review them.",
                        )),
                )
            })
            .when(any_collision, |el| {
                el.child(
                    div()
                        .h_flex()
                        .items_center()
                        .gap_2()
                        .text_xs()
                        .child(
                            div()
                                .text_color(status_amber())
                                .child(t(lang, "同名スキルの衝突時の動作:", "On name collision:")),
                        )
                        .child(
                            Button::new("imp-pol-skip")
                                .xsmall()
                                .when(self.skills_ui.import_policy == ImportConflictPolicy::Skip, |b| b.primary())
                                .when(self.skills_ui.import_policy != ImportConflictPolicy::Skip, |b| b.ghost())
                                .label(t(lang, "スキップ", "Skip"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.skills_ui.import_policy = ImportConflictPolicy::Skip;
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new("imp-pol-rename")
                                .xsmall()
                                .when(self.skills_ui.import_policy == ImportConflictPolicy::Rename, |b| b.primary())
                                .when(self.skills_ui.import_policy != ImportConflictPolicy::Rename, |b| b.ghost())
                                .label(t(lang, "別名で保存", "Rename"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.skills_ui.import_policy = ImportConflictPolicy::Rename;
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new("imp-pol-overwrite")
                                .xsmall()
                                .when(self.skills_ui.import_policy == ImportConflictPolicy::Overwrite, |b| b.danger())
                                .when(self.skills_ui.import_policy != ImportConflictPolicy::Overwrite, |b| b.ghost())
                                .label(t(lang, "上書き", "Overwrite"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.skills_ui.import_policy = ImportConflictPolicy::Overwrite;
                                    cx.notify();
                                })),
                        ),
                )
            })
            .child(
                div()
                    .v_flex()
                    .gap_1()
                    .max_h(px(140.0))
                    .overflow_y_scrollbar()
                    .children(preview.candidates.iter().map(|c| {
                        div()
                            .h_flex()
                            .items_center()
                            .justify_between()
                            .px_2()
                            .py_1()
                            .rounded(px(4.0))
                            .bg(theme.background)
                            .border_1()
                            .border_color(theme.border)
                            .text_xs()
                            .child(
                                div()
                                    .v_flex()
                                    .min_w_0()
                                    .flex_1()
                                    .child(
                                        div()
                                            .font_weight(FontWeight::MEDIUM)
                                            .child(c.name.clone()),
                                    )
                                    .child(
                                        div()
                                            .text_color(theme.muted_foreground)
                                            .truncate()
                                            .child(if !c.validation.is_valid() {
                                                format!("Error: {}", c.validation.error_summary())
                                            } else {
                                                c.description.clone()
                                            }),
                                    ),
                            )
                            .child(
                                div()
                                    .h_flex()
                                    .gap_1()
                                    .flex_shrink_0()
                                    .when(!c.related_files.is_empty(), |el| {
                                        el.child(
                                            div()
                                                .px_1()
                                                .rounded(px(3.0))
                                                .bg(theme.secondary)
                                                .child(format!("{} files", c.related_files.len())),
                                        )
                                    })
                                    .when(c.has_executable_scripts, |el| {
                                        el.child(
                                            div()
                                                .px_1()
                                                .rounded(px(3.0))
                                                .bg(status_amber().opacity(0.2))
                                                .text_color(status_amber())
                                                .child("scripts"),
                                        )
                                    })
                                    .when(c.collides_with_existing, |el| {
                                        el.child(
                                            div()
                                                .px_1()
                                                .rounded(px(3.0))
                                                .bg(status_red().opacity(0.2))
                                                .text_color(status_red())
                                                .child(t(lang, "衝突", "exists")),
                                        )
                                    }),
                            )
                    })),
            )
            .child(
                div()
                    .h_flex()
                    .justify_end()
                    .gap_1p5()
                    .child(
                        Button::new("btn-imp-cancel")
                            .xsmall()
                            .ghost()
                            .label(t(lang, "キャンセル", "Cancel"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.skills_ui.import_preview = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("btn-imp-confirm")
                            .xsmall()
                            .primary()
                            .label(t(lang, "インポート実行", "Import"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.confirm_import_preview(window, cx);
                                cx.notify();
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_skills_list_pane(
        &self,
        managed_entries: &[same_core::ResolvedSkillEntry],
        external_items: &[ExternalSkill],
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let lang = self.config.language;
        let current_pid = self.selected_project_id.clone();

        div()
            .v_flex()
            .w(px(340.0))
            .flex_shrink_0()
            .h_full()
            .border_r_1()
            .border_color(theme.border)
            .p_2p5()
            .gap_2()
            // Filter tabs
            .child(
                div()
                    .h_flex()
                    .gap_1()
                    .children([
                        (SkillFilterTab::All, t(lang, "すべて", "All")),
                        (SkillFilterTab::Global, "Global"),
                        (SkillFilterTab::Project, "Project"),
                        (SkillFilterTab::External, t(lang, "外部", "External")),
                    ].into_iter().map(|(tab, label)| {
                        let active = self.skills_ui.filter_tab == tab;
                        Button::new(SharedString::from(format!("skill-tab-{label}")))
                            .xsmall()
                            .when(active, |b| b.primary())
                            .when(!active, |b| b.ghost())
                            .label(label)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.skills_ui.filter_tab = tab;
                                cx.notify();
                            }))
                    })),
            )
            // Search box
            .child(Input::new(&self.skills_ui.search_input).small())
            // Scrollable list
            .child(
                div()
                    .v_flex()
                    .flex_1()
                    .min_h_0()
                    .gap_1()
                    .overflow_y_scrollbar()
                    .children(managed_entries.iter().map(|entry| {
                        let skill = &entry.skill;
                        let sid = skill.id.clone();
                        let sid_toggle = skill.id.clone();
                        let sid_reset = skill.id.clone();
                        let pid_for_override = current_pid.clone();
                        let is_selected = self.skills_ui.selected.as_ref()
                            == Some(&SelectedSkillItem::Managed(skill.id.clone()));
                        let is_project_context =
                            self.skills_ui.filter_tab == SkillFilterTab::All && current_pid.is_some();
                        let is_global = skill.scope == SkillScope::Global;

                        let (status_label, status_color): (&str, Hsla) = match entry.state {
                            EffectiveSkillState::Active => (
                                t(lang, "有効", "Active"),
                                status_green().into(),
                            ),
                            EffectiveSkillState::Disabled => (
                                t(lang, "無効", "Disabled"),
                                theme.muted_foreground,
                            ),
                            EffectiveSkillState::DisabledInProject => (
                                t(lang, "Projectで無効", "Off in Project"),
                                status_amber().into(),
                            ),
                            EffectiveSkillState::ShadowedByProject => (
                                t(lang, "Project優先", "Shadowed"),
                                status_amber().into(),
                            ),
                            EffectiveSkillState::ShadowedByDisabledProject => (
                                t(lang, "Project無効で停止", "Blocked"),
                                status_red().into(),
                            ),
                        };

                        div()
                            .id(SharedString::from(format!("skill-row-{}", skill.id)))
                            .v_flex()
                            .p_2()
                            .gap_1()
                            .rounded(px(6.0))
                            .border_1()
                            .border_color(if is_selected { accent().into() } else { theme.border })
                            .bg(if is_selected {
                                theme.list_active
                            } else {
                                theme.background
                            })
                            .hover(|s| s.bg(theme.list_hover))
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.select_managed_skill(sid.clone(), window, cx);
                                cx.notify();
                            }))
                            .child(
                                div()
                                    .h_flex()
                                    .items_center()
                                    .justify_between()
                                    .gap_1()
                                    .child(
                                        div()
                                            .h_flex()
                                            .items_center()
                                            .gap_1()
                                            .min_w_0()
                                            .flex_1()
                                            .child(
                                                div()
                                                    .text_sm()
                                                    .font_weight(FontWeight::MEDIUM)
                                                    .truncate()
                                                    .child(skill.name.clone()),
                                            )
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .px_1()
                                                    .rounded(px(3.0))
                                                    .bg(theme.secondary)
                                                    .text_color(theme.secondary_foreground)
                                                    .child(if is_global { "Global" } else { "Project" }),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(status_color)
                                            .flex_shrink_0()
                                            .child(status_label),
                                    ),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .truncate()
                                    .child(skill.description.clone()),
                            )
                            .child(
                                div()
                                    .h_flex()
                                    .items_center()
                                    .justify_between()
                                    .gap_1()
                                    .child(
                                        div()
                                            .h_flex()
                                            .items_center()
                                            .gap_1()
                                            .when(entry.project_override.is_some(), |el| {
                                                el.child(
                                                    div()
                                                        .text_xs()
                                                        .px_1()
                                                        .rounded(px(3.0))
                                                        .bg(accent().opacity(0.15))
                                                        .text_color(accent_text())
                                                        .child(t(lang, "上書き", "Override")),
                                                )
                                            })
                                            .when(is_global && entry.project_override.is_none() && is_project_context, |el| {
                                                el.child(
                                                    div()
                                                        .text_xs()
                                                        .text_color(theme.muted_foreground)
                                                        .child(t(lang, "継承", "Inherited")),
                                                )
                                            })
                                            .when(entry.has_external_conflict, |el| {
                                                el.child(
                                                    div()
                                                        .text_xs()
                                                        .px_1()
                                                        .rounded(px(3.0))
                                                        .bg(status_amber().opacity(0.2))
                                                        .text_color(status_amber())
                                                        .child(t(lang, "外部衝突", "Conflict")),
                                                )
                                            }),
                                    )
                                    .child(
                                        div()
                                            .h_flex()
                                            .items_center()
                                            .gap_1()
                                            .when(is_project_context && is_global && entry.project_override.is_some(), |el| {
                                                let pid_reset = pid_for_override.clone();
                                                el.child(
                                                    Button::new(SharedString::from(format!("skill-reset-{sid_reset}")))
                                                        .xsmall()
                                                        .ghost()
                                                        .label(t(lang, "継承に戻す", "Inherit"))
                                                        .on_click(cx.listener(move |this, _, _, cx| {
                                                            cx.stop_propagation();
                                                            if let Some(pid) = &pid_reset {
                                                                let _ = this.skills_ui.manager.clear_project_assignment(&sid_reset, pid);
                                                                this.reload_skills_state();
                                                                this.trigger_skills_sync_all(false);
                                                                cx.notify();
                                                            }
                                                        })),
                                                )
                                            })
                                            .child({
                                                let effective_on = entry.state == EffectiveSkillState::Active;
                                                let pid_t = pid_for_override.clone();
                                                Button::new(SharedString::from(format!("skill-tog-{sid_toggle}")))
                                                    .xsmall()
                                                    .ghost()
                                                    .label(if effective_on {
                                                        t(lang, "無効化", "Disable")
                                                    } else {
                                                        t(lang, "有効化", "Enable")
                                                    })
                                                    .on_click(cx.listener(move |this, _, _, cx| {
                                                        cx.stop_propagation();
                                                        if is_project_context && is_global {
                                                            if let Some(pid) = &pid_t {
                                                                let _ = this.skills_ui.manager.set_project_assignment(&sid_toggle, pid, !effective_on);
                                                            }
                                                        } else {
                                                            let _ = this.skills_ui.manager.set_skill_enabled(&sid_toggle, !effective_on);
                                                        }
                                                        this.reload_skills_state();
                                                        this.trigger_skills_sync_all(false);
                                                        cx.notify();
                                                    }))
                                            }),
                                    ),
                            )
                    }))
                    // External / Built-in section
                    .when(!external_items.is_empty(), |el| {
                        el.child(
                            div()
                                .pt_2()
                                .pb_0p5()
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme.muted_foreground)
                                .child(t(lang, "外部 / ビルトインスキル (読み取り専用)", "EXTERNAL / BUILT-IN SKILLS (READ-ONLY)")),
                        )
                        .children(external_items.iter().map(|ext| {
                            let path_sel = ext.path.clone();
                            let is_selected = self.skills_ui.selected.as_ref()
                                == Some(&SelectedSkillItem::External(ext.path.clone()));
                            let origin_badge = match ext.origin {
                                SkillOrigin::BuiltIn => "Built-in",
                                _ => "External",
                            };
                            div()
                                .id(SharedString::from(format!("ext-skill-{}", ext.path)))
                                .v_flex()
                                .p_2()
                                .gap_0p5()
                                .rounded(px(6.0))
                                .border_1()
                                .border_color(if is_selected { accent().into() } else { theme.border })
                                .bg(if is_selected {
                                    theme.list_active
                                } else {
                                    theme.background
                                })
                                .hover(|s| s.bg(theme.list_hover))
                                .cursor_pointer()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.select_external_skill(path_sel.clone());
                                    cx.notify();
                                }))
                                .child(
                                    div()
                                        .h_flex()
                                        .items_center()
                                        .justify_between()
                                        .gap_1()
                                        .child(
                                            div()
                                                .text_sm()
                                                .font_weight(FontWeight::MEDIUM)
                                                .truncate()
                                                .child(ext.name.clone()),
                                        )
                                        .child(
                                            div()
                                                .text_xs()
                                                .px_1()
                                                .rounded(px(3.0))
                                                .bg(theme.secondary)
                                                .text_color(theme.muted_foreground)
                                                .child(format!("{origin_badge} · {}", ext.provider_id)),
                                        ),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .truncate()
                                        .child(ext.source_label.clone()),
                                )
                        }))
                    })
                    .when(managed_entries.is_empty() && external_items.is_empty(), |el| {
                        el.child(
                            div()
                                .p_4()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(t(
                                    lang,
                                    "スキルがありません。「+ 新規作成」または「インポート」から追加できます。",
                                    "No skills found. Use '+ New Skill' or 'Import' to add one.",
                                )),
                        )
                    }),
            )
    }

    fn render_skill_detail_pane(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let lang = self.config.language;

        match &self.skills_ui.selected {
            Some(SelectedSkillItem::Creating) => {
                self.render_create_skill_pane(cx).into_any_element()
            }

            None => div()
                .v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .p_6()
                .gap_2()
                .child(div().text_sm().text_color(theme.muted_foreground).child(t(
                    lang,
                    "左の一覧からスキルを選択して詳細・編集・同期状態を確認できます",
                    "Select a skill from the list to view, edit, and inspect sync status",
                )))
                .into_any_element(),

            Some(SelectedSkillItem::Managed(skill_id)) => {
                let Some(skill) = self.skills_ui.skills.iter().find(|s| &s.id == skill_id) else {
                    return div().flex_1().into_any_element();
                };
                let sid = skill.id.clone();
                let skill_path = skill.path.clone();
                let skill_name = skill.name.clone();
                let skill_scope = skill.scope;
                let has_project = self.selected_project_id.is_some();
                let editor_text = self.skills_ui.editor_input.read(cx).value().to_string();
                let current_project_db_key = self.selected_project_id.as_deref().unwrap_or("");

                let deployments: Vec<_> = self
                    .skills_ui
                    .deployments
                    .iter()
                    .filter(|d| d.skill_id == skill.id && d.project_id == current_project_db_key)
                    .cloned()
                    .collect();

                div()
                    .v_flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .p_4()
                    .gap_3()
                    .overflow_y_scrollbar()
                    // Header
                    .child(
                        div()
                            .h_flex()
                            .items_center()
                            .justify_between()
                            .gap_2()
                            .child(
                                div()
                                    .v_flex()
                                    .min_w_0()
                                    .flex_1()
                                    .gap_0p5()
                                    .child(
                                        div()
                                            .h_flex()
                                            .items_center()
                                            .gap_1p5()
                                            .child(
                                                div()
                                                    .text_lg()
                                                    .font_weight(FontWeight::BOLD)
                                                    .child(if self.skills_ui.editor_dirty {
                                                        format!("{skill_name} *")
                                                    } else {
                                                        skill_name.clone()
                                                    }),
                                            )
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .px_1p5()
                                                    .py(px(1.0))
                                                    .rounded(px(4.0))
                                                    .bg(theme.secondary)
                                                    .child(if skill_scope == SkillScope::Global {
                                                        "Global"
                                                    } else {
                                                        "Project"
                                                    }),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .truncate()
                                            .child(skill_path.clone()),
                                    ),
                            )
                            .child(
                                div()
                                    .h_flex()
                                    .items_center()
                                    .gap_1()
                                    .flex_shrink_0()
                                    .child(
                                        Button::new("btn-skill-save")
                                            .xsmall()
                                            .primary()
                                            .disabled(!self.skills_ui.editor_dirty)
                                            .label(t(lang, "保存", "Save"))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.save_selected_skill(cx);
                                                cx.notify();
                                            })),
                                    )
                                    .child(
                                        Button::new("btn-skill-preview-toggle")
                                            .xsmall()
                                            .outline()
                                            .label(if self.skills_ui.preview_mode {
                                                t(lang, "編集", "Edit")
                                            } else {
                                                t(lang, "プレビュー", "Preview")
                                            })
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.skills_ui.preview_mode =
                                                    !this.skills_ui.preview_mode;
                                                cx.notify();
                                            })),
                                    )
                                    .child({
                                        let p = skill_path.clone();
                                        Button::new("btn-skill-open-folder")
                                            .xsmall()
                                            .ghost()
                                            .label(t(lang, "フォルダを開く", "Open Folder"))
                                            .on_click(cx.listener(move |_, _, _, _| {
                                                open_path_in_file_manager(Path::new(&p));
                                            }))
                                    })
                                    .child(
                                        Button::new("btn-skill-export")
                                            .xsmall()
                                            .ghost()
                                            .label(t(lang, "エクスポート", "Export"))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.export_selected_skill(cx);
                                            })),
                                    ),
                            ),
                    )
                    // Secondary action bar (Rename, Duplicate, Move Scope, Delete)
                    .child(
                        div()
                            .h_flex()
                            .items_center()
                            .gap_1p5()
                            .child({
                                let cur_name = skill_name.clone();
                                Button::new("btn-skill-rename-open")
                                    .xsmall()
                                    .ghost()
                                    .label(t(lang, "名前変更", "Rename"))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.skills_ui.renaming = !this.skills_ui.renaming;
                                        if this.skills_ui.renaming {
                                            this.skills_ui.rename_input.update(cx, |s, cx| {
                                                s.set_value(cur_name.clone(), window, cx);
                                            });
                                        }
                                        cx.notify();
                                    }))
                            })
                            .child({
                                let dup_id = sid.clone();
                                let dup_base = skill_name.clone();
                                let dup_scope = skill_scope;
                                Button::new("btn-skill-dup")
                                    .xsmall()
                                    .ghost()
                                    .label(t(lang, "複製", "Duplicate"))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        let new_name = format!("{dup_base}-copy");
                                        let pid = match dup_scope {
                                            SkillScope::Global => None,
                                            SkillScope::Project => this.selected_project_id.clone(),
                                        };
                                        match this.skills_ui.manager.duplicate_skill(
                                            &dup_id,
                                            &new_name,
                                            dup_scope,
                                            pid.as_deref(),
                                        ) {
                                            Ok(copy) => {
                                                this.reload_skills_state();
                                                this.select_managed_skill(copy.id, window, cx);
                                                this.trigger_skills_sync_all(false);
                                            }
                                            Err(e) => {
                                                this.pending_notes
                                                    .push((NoteKind::Error, format!("{e:#}")));
                                            }
                                        }
                                        cx.notify();
                                    }))
                            })
                            .child({
                                let move_id = sid.clone();
                                let target_scope = match skill_scope {
                                    SkillScope::Global => SkillScope::Project,
                                    SkillScope::Project => SkillScope::Global,
                                };
                                Button::new("btn-skill-move")
                                    .xsmall()
                                    .ghost()
                                    .disabled(target_scope == SkillScope::Project && !has_project)
                                    .label(match target_scope {
                                        SkillScope::Project => {
                                            t(lang, "Projectへ移動", "Move to Project")
                                        }
                                        SkillScope::Global => {
                                            t(lang, "Globalへ移動", "Move to Global")
                                        }
                                    })
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        let pid = match target_scope {
                                            SkillScope::Global => None,
                                            SkillScope::Project => this.selected_project_id.clone(),
                                        };
                                        match this.skills_ui.manager.move_skill(
                                            &move_id,
                                            target_scope,
                                            pid.as_deref(),
                                        ) {
                                            Ok(moved) => {
                                                this.reload_skills_state();
                                                this.select_managed_skill(moved.id, window, cx);
                                                this.trigger_skills_sync_all(false);
                                            }
                                            Err(e) => {
                                                this.pending_notes
                                                    .push((NoteKind::Error, format!("{e:#}")));
                                            }
                                        }
                                        cx.notify();
                                    }))
                            })
                            .child({
                                let del_id = sid.clone();
                                let key = format!("skill:{del_id}");
                                let armed = self.confirm_delete.as_deref() == Some(key.as_str());
                                Button::new("btn-skill-delete")
                                    .xsmall()
                                    .when(armed, |b| b.danger())
                                    .when(!armed, |b| b.ghost())
                                    .label(if armed {
                                        t(lang, "本当に削除?", "Confirm Delete?")
                                    } else {
                                        t(lang, "削除", "Delete")
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if this.confirm_or_arm_delete(key.clone(), cx) {
                                            match this.skills_ui.manager.delete_skill(&del_id) {
                                                Ok(()) => {
                                                    this.skills_ui.selected = None;
                                                    this.reload_skills_state();
                                                    this.trigger_skills_sync_all(false);
                                                }
                                                Err(e) => {
                                                    this.pending_notes
                                                        .push((NoteKind::Error, format!("{e:#}")));
                                                }
                                            }
                                        }
                                        cx.notify();
                                    }))
                            }),
                    )
                    // Inline rename bar
                    .when(self.skills_ui.renaming, |el| {
                        let ren_id = sid.clone();
                        el.child(
                            div()
                                .h_flex()
                                .gap_1p5()
                                .items_center()
                                .child(
                                    div()
                                        .w(px(240.0))
                                        .child(Input::new(&self.skills_ui.rename_input).small()),
                                )
                                .child(
                                    Button::new("btn-skill-rename-apply")
                                        .xsmall()
                                        .primary()
                                        .label(t(lang, "適用", "Apply"))
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            let next = this
                                                .skills_ui
                                                .rename_input
                                                .read(cx)
                                                .value()
                                                .trim()
                                                .to_string();
                                            match this
                                                .skills_ui
                                                .manager
                                                .rename_skill(&ren_id, &next)
                                            {
                                                Ok(updated) => {
                                                    this.skills_ui.renaming = false;
                                                    this.reload_skills_state();
                                                    this.select_managed_skill(
                                                        updated.id, window, cx,
                                                    );
                                                    this.trigger_skills_sync_all(false);
                                                }
                                                Err(e) => {
                                                    this.pending_notes
                                                        .push((NoteKind::Error, format!("{e:#}")));
                                                }
                                            }
                                            cx.notify();
                                        })),
                                ),
                        )
                    })
                    // Validation error banner
                    .when_some(self.skills_ui.validation_error.clone(), |el, err| {
                        el.child(
                            div()
                                .text_xs()
                                .p_2()
                                .rounded(px(4.0))
                                .bg(status_red().opacity(0.15))
                                .text_color(status_red())
                                .child(err),
                        )
                    })
                    // SKILL.md Editor or Markdown Preview
                    .child(
                        div()
                            .v_flex()
                            .h(px(320.0))
                            .border_1()
                            .border_color(theme.border)
                            .rounded(px(6.0))
                            .overflow_hidden()
                            .child(if self.skills_ui.preview_mode {
                                div()
                                    .p_3()
                                    .size_full()
                                    .overflow_y_scrollbar()
                                    .child(
                                        TextView::markdown("skill-md-preview", editor_text)
                                            .selectable(true),
                                    )
                                    .into_any_element()
                            } else {
                                Textarea::new(&self.skills_ui.editor_input)
                                    .h_full()
                                    .into_any_element()
                            }),
                    )
                    // Related files section
                    .child(
                        div()
                            .v_flex()
                            .gap_1()
                            .child(
                                div()
                                    .h_flex()
                                    .items_center()
                                    .justify_between()
                                    .child(
                                        div()
                                            .text_xs()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(theme.muted_foreground)
                                            .child(t(
                                                lang,
                                                "関連ファイル (scripts / references / assets)",
                                                "RELATED FILES",
                                            )),
                                    )
                                    .child(
                                        Button::new("btn-skill-add-file")
                                            .xsmall()
                                            .ghost()
                                            .label(t(lang, "+ ファイル追加", "+ Add File"))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.add_related_file_to_selected(cx);
                                            })),
                                    ),
                            )
                            .when(self.skills_ui.related_files.is_empty(), |el| {
                                el.child(
                                    div().text_xs().text_color(theme.muted_foreground).child(t(
                                        lang,
                                        "関連ファイルはありません",
                                        "No related files",
                                    )),
                                )
                            })
                            .children(self.skills_ui.related_files.iter().map(|rf| {
                                let del_sid = sid.clone();
                                let rel = rf.relative_path.clone();
                                div()
                                    .h_flex()
                                    .items_center()
                                    .justify_between()
                                    .px_2()
                                    .py_1()
                                    .rounded(px(4.0))
                                    .bg(theme.sidebar)
                                    .text_xs()
                                    .child(
                                        div()
                                            .h_flex()
                                            .items_center()
                                            .gap_1p5()
                                            .child(rf.relative_path.clone())
                                            .when(rf.is_executable, |el| {
                                                el.child(
                                                    div()
                                                        .px_1()
                                                        .rounded(px(3.0))
                                                        .bg(status_amber().opacity(0.2))
                                                        .text_color(status_amber())
                                                        .child("executable"),
                                                )
                                            }),
                                    )
                                    .child(
                                        Button::new(SharedString::from(format!(
                                            "rm-rf-{}",
                                            rf.relative_path
                                        )))
                                        .xsmall()
                                        .ghost()
                                        .label("✕")
                                        .on_click(
                                            cx.listener(move |this, _, _, cx| {
                                                if this
                                                    .skills_ui
                                                    .manager
                                                    .delete_related_file(&del_sid, &rel)
                                                    .is_ok()
                                                {
                                                    this.skills_ui.related_files = this
                                                        .skills_ui
                                                        .manager
                                                        .list_related_files(&del_sid)
                                                        .unwrap_or_default();
                                                    this.reload_skills_state();
                                                    this.trigger_skills_sync_all(false);
                                                }
                                                cx.notify();
                                            }),
                                        ),
                                    )
                            })),
                    )
                    // Agent deployment status section
                    .child(
                        div()
                            .v_flex()
                            .gap_1()
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme.muted_foreground)
                                    .child(t(
                                        lang,
                                        "エージェント別提供状態",
                                        "AGENT DEPLOYMENT STATUS",
                                    )),
                            )
                            .children(self.config.providers.iter().map(|provider| {
                                let dep = deployments.iter().find(|d| d.provider_id == provider.id);
                                let method = provider_skill_method(provider);
                                let (st_label, st_color): (&str, Hsla) = match dep.map(|d| d.status)
                                {
                                    Some(SkillDeploymentStatus::Synced) => {
                                        ("Synced", status_green().into())
                                    }
                                    Some(SkillDeploymentStatus::Disabled) => {
                                        ("Disabled", theme.muted_foreground)
                                    }
                                    Some(SkillDeploymentStatus::Conflict) => {
                                        ("Conflict", status_amber().into())
                                    }
                                    Some(SkillDeploymentStatus::Unsupported) => {
                                        ("Unsupported", theme.muted_foreground)
                                    }
                                    Some(SkillDeploymentStatus::RestartRequired) => {
                                        ("Restart Required", status_amber().into())
                                    }
                                    Some(SkillDeploymentStatus::Error) => {
                                        ("Error", status_red().into())
                                    }
                                    Some(SkillDeploymentStatus::Syncing) => {
                                        ("Syncing", accent_text().into())
                                    }
                                    Some(SkillDeploymentStatus::Available) | None => {
                                        ("Available", theme.muted_foreground)
                                    }
                                };
                                let detail = dep
                                    .and_then(|d| d.last_error.clone())
                                    .unwrap_or_else(|| method.label().to_string());
                                div()
                                    .h_flex()
                                    .items_center()
                                    .justify_between()
                                    .px_2()
                                    .py_1()
                                    .rounded(px(4.0))
                                    .bg(theme.sidebar)
                                    .text_xs()
                                    .child(
                                        div()
                                            .h_flex()
                                            .items_center()
                                            .gap_2()
                                            .child(
                                                div()
                                                    .font_weight(FontWeight::MEDIUM)
                                                    .child(provider.name.clone()),
                                            )
                                            .child(
                                                div()
                                                    .text_color(theme.muted_foreground)
                                                    .truncate()
                                                    .child(detail),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .font_weight(FontWeight::MEDIUM)
                                            .text_color(st_color)
                                            .child(st_label),
                                    )
                            })),
                    )
                    .into_any_element()
            }

            Some(SelectedSkillItem::External(ext_path)) => {
                let Some(ext) = self
                    .skills_ui
                    .external_skills
                    .iter()
                    .find(|e| &e.path == ext_path)
                else {
                    return div().flex_1().into_any_element();
                };
                let p_open = ext.path.clone();
                let p_imp_g = ext.path.clone();
                let p_imp_p = ext.path.clone();
                let has_project = self.selected_project_id.is_some();

                div()
                    .v_flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .p_4()
                    .gap_3()
                    .overflow_y_scrollbar()
                    .child(
                        div()
                            .h_flex()
                            .items_center()
                            .justify_between()
                            .gap_2()
                            .child(
                                div()
                                    .v_flex()
                                    .min_w_0()
                                    .flex_1()
                                    .child(
                                        div()
                                            .text_lg()
                                            .font_weight(FontWeight::BOLD)
                                            .child(ext.name.clone()),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child(format!(
                                                "{} · {} · {}",
                                                ext.source_label, ext.provider_id, ext.execution_target
                                            )),
                                    ),
                            )
                            .child(
                                div()
                                    .h_flex()
                                    .gap_1()
                                    .child(
                                        Button::new("btn-ext-imp-global")
                                            .xsmall()
                                            .primary()
                                            .label(t(lang, "Globalにインポート", "Import to Global"))
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.import_external_skill(
                                                    p_imp_g.clone(),
                                                    SkillScope::Global,
                                                    window,
                                                    cx,
                                                );
                                                cx.notify();
                                            })),
                                    )
                                    .child(
                                        Button::new("btn-ext-imp-project")
                                            .xsmall()
                                            .outline()
                                            .disabled(!has_project)
                                            .label(t(lang, "Projectにインポート", "Import to Project"))
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.import_external_skill(
                                                    p_imp_p.clone(),
                                                    SkillScope::Project,
                                                    window,
                                                    cx,
                                                );
                                                cx.notify();
                                            })),
                                    )
                                    .child(
                                        Button::new("btn-ext-open")
                                            .xsmall()
                                            .ghost()
                                            .label(t(lang, "フォルダを開く", "Open Folder"))
                                            .on_click(cx.listener(move |_, _, _, _| {
                                                open_path_in_file_manager(Path::new(&p_open));
                                            })),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .p_2()
                            .rounded(px(4.0))
                            .bg(theme.sidebar)
                            .text_color(theme.muted_foreground)
                            .child(t(
                                lang,
                                "外部エージェントまたはリポジトリが管理するスキルです。SAMEは外部スキルを無断で変更・削除しません（無効化の強制は各エージェント側の設定に依存します）。",
                                "Managed externally by an agent or repository. SAME treats external skills as read-only and never modifies or deletes them.",
                            )),
                    )
                    .child(
                        div()
                            .p_3()
                            .rounded(px(6.0))
                            .border_1()
                            .border_color(theme.border)
                            .child(
                                TextView::markdown(
                                    "ext-skill-md-preview",
                                    self.skills_ui.external_md_preview.clone(),
                                )
                                .selectable(true),
                            ),
                    )
                    .into_any_element()
            }
        }
    }
}

/// Opens a directory in the OS file manager (Windows Explorer / macOS Finder / Linux xdg-open).
pub(super) fn open_path_in_file_manager(path: &Path) {
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("explorer").arg(path).spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(path).spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    }
}

#[cfg(test)]
mod prompt_skill_tests {
    use super::*;

    #[test]
    fn detects_trailing_dollar_skill_token() {
        assert_eq!(detect_dollar_skill_token("$"), Some((0, "")));
        assert_eq!(
            detect_dollar_skill_token("$code-rev"),
            Some((0, "code-rev"))
        );
        assert_eq!(
            detect_dollar_skill_token("PRを見て $sec"),
            Some(("PRを見て ".len(), "sec"))
        );
        assert_eq!(detect_dollar_skill_token("price is 10$"), None);
        assert_eq!(detect_dollar_skill_token("$done next"), None);
    }

    #[test]
    fn extracts_multiple_dollar_mentions() {
        assert_eq!(
            extract_dollar_skill_names("$rust-review と $security-check で確認して $rust-review"),
            vec!["rust-review".to_string(), "security-check".to_string()]
        );
    }
}
