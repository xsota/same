use gpui_component::Selectable;
use crate::i18n::t;
use crate::theme::{
    accent, accent_foreground, accent_text, apply_gpui_theme, default_font_size,
    default_mono_font, default_ui_font, status_amber, status_green, status_red,
};
use crate::workspace_fs::{load_git_info, scan_directory, FileEntry, GitInfo};
use chrono::{DateTime, Local, Utc};
use gpui::{
    actions, div, list, prelude::FluentBuilder as _, px, AnyElement, App, AppContext, ClickEvent,
    ClipboardItem, Context, Div, ElementId, Entity, FocusHandle, Focusable, FollowMode, FontWeight,
    Hsla, InteractiveElement, IntoElement, KeyBinding, ListAlignment, ListState, ParentElement,
    PathPromptOptions, Render, SharedString, Stateful, StatefulInteractiveElement, Styled,
    Subscription, Window,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_component::notification::Notification;
use gpui_component::text::TextView;
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _, Theme,
    TitleBar, WindowExt as _,
};
use same_agent::{
    exec as agent_exec, AgentManager, ExecPreference, PromptRequest, ProviderCheckResult,
    ProviderSetup, SetupOutcome,
};
use same_core::paths::simplify_path;
use same_core::{
    AccentColor, AppConfig, AppPaths, EventBus, ExecMode, Language, Message, MessageRole, ModelInfo,
    SubmitKey, MAX_FONT_SIZE, MIN_FONT_SIZE,
    PermissionRequest, Project, ProviderConfig, ProviderKind, Quest, SameEvent, Session,
    SessionStatus, SlashCommandInfo, Storage, ThemeMode, ToolCallRecord, ToolCallStatus, Workspace,
};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use uuid::Uuid;

actions!(
    same,
    [NewSession, SaveFile, FocusPrompt, ToggleRightPanel, SendPrompt]
);

/// Registers the application-wide keyboard shortcuts.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("ctrl-n", NewSession, None),
        KeyBinding::new("ctrl-s", SaveFile, None),
        KeyBinding::new("ctrl-l", FocusPrompt, None),
        KeyBinding::new("ctrl-b", ToggleRightPanel, None),
        // Sends the prompt when the send key is Ctrl/Cmd+Enter, in case the
        // textarea does not report that chord itself.
        KeyBinding::new(
            if cfg!(target_os = "macos") {
                "cmd-enter"
            } else {
                "ctrl-enter"
            },
            SendPrompt,
            None,
        ),
    ]);
}

const DEFAULT_SESSION_TITLE: &str = "New Session";
const MAX_OPEN_FILE_BYTES: u64 = 2 * 1024 * 1024;
const TOOL_OUTPUT_PREVIEW_CHARS: usize = 4000;
/// Separates a failed run's error from the partial reply in a stored message.
/// An HTML comment, so it stays invisible if the text is shown as markdown.
const ERROR_MARKER: &str = "\n\n<!--same:error-->";

#[derive(Default)]
struct LiveDraft {
    content: String,
    thinking: String,
    tool_calls: Vec<ToolCallRecord>,
}

enum UiAsyncMsg {
    Event(SameEvent),
    /// A provider check, tagged with the check generation it belongs to.
    ProviderCheck(u64, ProviderCheckResult),
    ConnectResult {
        provider_id: String,
        ok: bool,
        message: String,
    },
    SetupDone {
        provider_id: String,
        result: Result<SetupOutcome, String>,
    },
    ModelsDiscovered {
        provider_id: String,
        target: String,
        result: Result<same_agent::discovery::Discovered, String>,
        /// Started by the user (show the outcome), not in the background.
        manual: bool,
    },
}

/// File (in the data dir) that keeps the last model list of each provider,
/// so the picker is complete right after launch.
const MODELS_CACHE_FILE: &str = "models_cache.json";

#[derive(PartialEq, Eq, Clone, Copy)]
enum MainView {
    Chat,
    Settings,
}

/// A page of the settings screen.
#[derive(PartialEq, Eq, Clone, Copy)]
enum SettingsSection {
    General,
    Appearance,
    Providers,
}

impl SettingsSection {
    const ALL: [SettingsSection; 3] = [
        SettingsSection::General,
        SettingsSection::Appearance,
        SettingsSection::Providers,
    ];

    fn title(self, lang: Language) -> &'static str {
        match self {
            SettingsSection::General => t(lang, "一般", "General"),
            SettingsSection::Appearance => t(lang, "外観", "Appearance"),
            SettingsSection::Providers => t(lang, "プロバイダ接続", "Providers"),
        }
    }

    fn icon(self) -> IconName {
        match self {
            SettingsSection::General => IconName::Settings2,
            SettingsSection::Appearance => IconName::Palette,
            SettingsSection::Providers => IconName::Bot,
        }
    }
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum RightTab {
    Files,
    Git,
    Logs,
}

#[derive(Clone, Copy)]
enum NoteKind {
    Info,
    Success,
    Warning,
    Error,
}

pub struct SameAppView {
    rt: Arc<tokio::runtime::Runtime>,
    paths: AppPaths,
    config: AppConfig,
    storage: Arc<Storage>,
    agent_manager: AgentManager,
    ui_tx: mpsc::UnboundedSender<UiAsyncMsg>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
    /// Subscription to `prompt_input`, replaced when the input is rebuilt.
    _prompt_sub: Subscription,

    active_view: MainView,
    settings_section: SettingsSection,
    workspace: Workspace,
    projects: Vec<Project>,
    selected_project_id: Option<String>,
    quests: Vec<Quest>,
    selected_quest_filter: Option<String>,
    sessions: Vec<Session>,
    selected_session_id: Option<String>,
    messages: Vec<Message>,

    live_drafts: HashMap<String, LiveDraft>,
    pending_permissions: Vec<PermissionRequest>,
    slash_commands: HashMap<String, Vec<SlashCommandInfo>>,
    /// Models an agent reported (ACP), by provider id, so a fresh session
    /// can offer them before its first prompt.
    agent_models: HashMap<String, Vec<ModelInfo>>,
    /// (provider id, exec target) pairs whose models were asked for this run.
    models_discovered: HashSet<(String, String)>,
    /// Providers with a model discovery in flight.
    models_discovering: HashSet<String>,
    /// Per provider: the model its agent uses when none is picked.
    agent_default_model: HashMap<String, String>,
    /// The model an agent says a session is using, by session id.
    agent_current_model: HashMap<String, String>,
    provider_checks: HashMap<String, ProviderCheckResult>,
    /// Bumped whenever checks are restarted (e.g. on project switch), so
    /// results for another project's environment are dropped.
    check_generation: u64,
    connect_results: HashMap<String, (bool, String)>,
    login_results: HashMap<String, String>,
    /// Providers with an install/sign-in terminal currently open.
    setup_running: HashSet<String>,
    expanded_thinking: HashSet<String>,
    expanded_tools: HashSet<String>,
    logs: Vec<String>,
    pending_notes: Vec<(NoteKind, String)>,
    /// Two-step delete: the key of the item whose delete button was clicked
    /// once and now asks for confirmation (e.g. "sess:<id>").
    confirm_delete: Option<String>,

    // Chat
    prompt_input: Entity<TextareaState>,
    prompt_empty: bool,
    /// The prompt is a `/command` being typed: show suggestions.
    prompt_is_command: bool,
    /// Virtualized chat feed: one item per stored message, plus a trailing
    /// item for the live reply / permission prompts / empty state. Only the
    /// items on screen are laid out, so long sessions stay cheap to render.
    feed: ListState,
    /// (session id, item count) the feed was last synced with.
    feed_synced: std::cell::RefCell<(Option<String>, usize)>,
    renaming_session_id: Option<String>,
    rename_input: Entity<InputState>,

    // Right panel
    show_right_panel: bool,
    right_tab: RightTab,
    file_filter_input: Entity<InputState>,
    file_entries: Vec<FileEntry>,
    expanded_dirs: HashSet<PathBuf>,
    opened_file_path: Option<PathBuf>,
    opened_file_saved: String,
    file_dirty: bool,
    reload_open_file_pending: bool,
    file_editor_input: Entity<TextareaState>,
    git_info: GitInfo,

    // Inline creators in sidebar
    show_new_project_form: bool,
    new_project_name: Entity<InputState>,
    new_project_path: Entity<InputState>,
    show_new_quest_form: bool,
    new_quest_title: Entity<InputState>,
    new_quest_goal: Entity<InputState>,

    // Provider edit form
    editing_provider_id: Option<String>,
    edit_provider_cmd: Entity<InputState>,
    edit_provider_args: Entity<InputState>,
    edit_provider_model: Entity<InputState>,

    // Custom ACP provider form
    custom_id: Entity<InputState>,
    custom_name: Entity<InputState>,
    custom_cmd: Entity<InputState>,
    custom_args: Entity<InputState>,
    custom_desc: Entity<InputState>,

    // Settings → Appearance font pickers
    /// Installed font families, sorted.
    font_names: Vec<String>,
    /// The open font dropdown: `Some(false)` UI font, `Some(true)` code font.
    font_menu_open: Option<bool>,
    /// Virtualized rows of the font dropdown, one per `font_rows` entry.
    /// Shared by both pickers (only one is open at a time).
    font_list: ListState,
    /// The dropdown's rows after filtering: `None` is the platform default,
    /// `Some(i)` is `font_names[i]`.
    font_rows: Vec<Option<usize>>,
    /// Search box at the top of the font dropdown.
    font_search: Entity<InputState>,
}

impl SameAppView {
    pub fn new(
        initial_dir: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<Self> {
        let paths = AppPaths::resolve()?;
        let agent_models = load_models_cache(&paths.data_dir);
        let config = AppConfig::load_or_default(&paths)?;
        let storage = Arc::new(Storage::open(&paths.db_file)?);
        // No agent process survives a restart: sessions persisted as running
        // would otherwise show a Stop button that can never do anything.
        let _ = storage.reset_active_sessions();

        apply_gpui_theme(&config, Some(window), cx);
        // Installed font families, for the font pickers in Settings.
        let mut font_names = cx.text_system().all_font_names();
        font_names.sort_by_key(|n| n.to_lowercase());
        font_names.dedup();

        let rt = Arc::new(tokio::runtime::Runtime::new()?);
        let events = EventBus::new(1024);
        let agent_manager = AgentManager::new(events.clone(), rt.handle().clone());
        agent_manager.set_auto_approve(config.auto_approve_tools);

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();

        // Forward EventBus -> UI async channel
        {
            let mut rx = events.subscribe();
            let ui_tx = ui_tx.clone();
            rt.spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(ev) => {
                            if ui_tx.send(UiAsyncMsg::Event(ev)).is_err() {
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
        }

        // Pump async messages into GPUI view state without polling
        cx.spawn(async move |this, cx| {
            while let Some(msg) = ui_rx.recv().await {
                let updated = this.update(cx, |view, cx| {
                    view.handle_async_msg(msg);
                    cx.notify();
                });
                if updated.is_err() {
                    break;
                }
            }
        })
        .detach();

        let workspace = storage.ensure_default_workspace()?;
        let mut projects = storage.list_projects(&workspace.id)?;

        let target_dir = initial_dir.or_else(|| {
            if projects.is_empty() {
                std::env::current_dir().ok()
            } else {
                None
            }
        });

        let selected_project_id = if let Some(dir) = target_dir {
            let proj = storage.ensure_project_for_path(&workspace.id, &dir)?;
            projects = storage.list_projects(&workspace.id)?;
            Some(proj.id)
        } else {
            projects.first().map(|p| p.id.clone())
        };

        let prompt_input = Self::new_prompt_input(config.submit_key, window, cx);
        let prompt_sub = cx.subscribe_in(&prompt_input, window, Self::on_prompt_event);
        let rename_input = cx.new(|cx| InputState::new(window, cx));
        let file_filter_input = cx.new(|cx| InputState::new(window, cx));
        let file_editor_input = cx.new(|cx| TextareaState::new(window, cx));

        let new_project_name = cx.new(|cx| InputState::new(window, cx));
        let new_project_path = cx.new(|cx| InputState::new(window, cx));
        let new_quest_title = cx.new(|cx| InputState::new(window, cx));
        let new_quest_goal = cx.new(|cx| InputState::new(window, cx));

        let edit_provider_cmd = cx.new(|cx| InputState::new(window, cx));
        let edit_provider_args = cx.new(|cx| InputState::new(window, cx));
        let edit_provider_model = cx.new(|cx| InputState::new(window, cx));

        let custom_id = cx.new(|cx| InputState::new(window, cx));
        let custom_name = cx.new(|cx| InputState::new(window, cx));
        let custom_cmd = cx.new(|cx| InputState::new(window, cx));
        let custom_args = cx.new(|cx| InputState::new(window, cx));
        let custom_desc = cx.new(|cx| InputState::new(window, cx));
        let font_search = cx.new(|cx| InputState::new(window, cx));

        let mut subscriptions = vec![
            cx.subscribe_in(&rename_input, window, |this, _, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                    this.commit_rename(cx);
                    cx.notify();
                }
            }),
            cx.subscribe_in(
                &file_filter_input,
                window,
                |this, _, ev: &InputEvent, _, cx| {
                    if matches!(ev, InputEvent::Change) {
                        this.reload_workspace_files(cx);
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(
                &file_editor_input,
                window,
                |this, state, ev: &InputEvent, _, cx| {
                    if matches!(ev, InputEvent::Change) {
                        let dirty = this.opened_file_path.is_some()
                            && state.read(cx).value().as_ref() != this.opened_file_saved.as_str();
                        if dirty != this.file_dirty {
                            this.file_dirty = dirty;
                            cx.notify();
                        }
                    }
                },
            ),
            cx.subscribe_in(
                &new_project_path,
                window,
                |this, _, ev: &InputEvent, window, cx| {
                    if matches!(ev, InputEvent::PressEnter { .. }) {
                        this.create_project_from_form(window, cx);
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(
                &new_quest_goal,
                window,
                |this, _, ev: &InputEvent, window, cx| {
                    if matches!(ev, InputEvent::PressEnter { .. }) {
                        this.create_quest_from_form(window, cx);
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(
                &new_quest_title,
                window,
                |this, _, ev: &InputEvent, window, cx| {
                    if matches!(ev, InputEvent::PressEnter { .. }) {
                        this.create_quest_from_form(window, cx);
                        cx.notify();
                    }
                },
            ),
            // Font dropdown search: typing filters the rows, Enter picks the
            // first match.
            cx.subscribe_in(
                &font_search,
                window,
                |this, _, ev: &InputEvent, window, cx| match ev {
                    InputEvent::Change => {
                        this.filter_font_rows(cx);
                        cx.notify();
                    }
                    InputEvent::PressEnter { .. } => {
                        if let (Some(mono), Some(&row)) =
                            (this.font_menu_open, this.font_rows.first())
                        {
                            let family = row.map(|i| this.font_names[i].clone());
                            this.set_font(mono, family, window, cx);
                            this.font_menu_open = None;
                        }
                        cx.notify();
                    }
                    _ => {}
                },
            ),
        ];

        // Stop agent processes on quit so none outlive the app.
        subscriptions.push(cx.on_app_quit(|this: &mut Self, _| {
            let mgr = this.agent_manager.clone();
            let running: Vec<String> = this.live_drafts.keys().cloned().collect();
            let task = this.rt.spawn(async move {
                for sid in running {
                    mgr.cancel_session(&sid).await;
                }
                mgr.shutdown_all().await;
            });
            async move {
                let _ = task.await;
            }
        }));

        let mut app = Self {
            rt,
            paths,
            config,
            storage,
            agent_manager,
            ui_tx,
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
            _prompt_sub: prompt_sub,
            active_view: MainView::Chat,
            settings_section: SettingsSection::General,
            workspace,
            projects,
            selected_project_id,
            quests: vec![],
            selected_quest_filter: None,
            sessions: vec![],
            selected_session_id: None,
            messages: vec![],
            live_drafts: HashMap::new(),
            pending_permissions: vec![],
            slash_commands: HashMap::new(),
            agent_models,
            models_discovered: HashSet::new(),
            models_discovering: HashSet::new(),
            agent_default_model: HashMap::new(),
            agent_current_model: HashMap::new(),
            provider_checks: HashMap::new(),
            check_generation: 0,
            connect_results: HashMap::new(),
            login_results: HashMap::new(),
            setup_running: HashSet::new(),
            expanded_thinking: HashSet::new(),
            expanded_tools: HashSet::new(),
            logs: vec![],
            pending_notes: vec![],
            confirm_delete: None,
            prompt_input,
            prompt_empty: true,
            prompt_is_command: false,
            feed: {
                let feed = ListState::new(0, ListAlignment::Top, px(1000.0));
                feed.set_follow_mode(FollowMode::Tail);
                feed
            },
            feed_synced: Default::default(),
            renaming_session_id: None,
            rename_input,
            show_right_panel: true,
            right_tab: RightTab::Files,
            file_filter_input,
            file_entries: vec![],
            expanded_dirs: HashSet::new(),
            opened_file_path: None,
            opened_file_saved: String::new(),
            file_dirty: false,
            reload_open_file_pending: false,
            file_editor_input,
            git_info: GitInfo::default(),
            show_new_project_form: false,
            new_project_name,
            new_project_path,
            show_new_quest_form: false,
            new_quest_title,
            new_quest_goal,
            editing_provider_id: None,
            edit_provider_cmd,
            edit_provider_args,
            edit_provider_model,
            custom_id,
            custom_name,
            custom_cmd,
            custom_args,
            custom_desc,
            font_list: ListState::new(font_names.len() + 1, ListAlignment::Top, px(200.0)),
            font_rows: std::iter::once(None)
                .chain((0..font_names.len()).map(Some))
                .collect(),
            font_names,
            font_menu_open: None,
            font_search,
        };

        app.apply_placeholders(window, cx);
        app.reload_project_state(window, cx);
        app.refresh_provider_checks();
        app.prompt_input.update(cx, |s, cx| s.focus(window, cx));
        Ok(app)
    }

    /// (Re)applies localized placeholders to every text input.
    fn apply_placeholders(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let lang = self.config.language;
        let set_input = |input: &Entity<InputState>,
                         text: &str,
                         window: &mut Window,
                         cx: &mut Context<Self>| {
            let text = text.to_string();
            input.update(cx, |s, cx| s.set_placeholder(text, window, cx));
        };
        self.prompt_input.update(cx, |s, cx| {
            s.set_placeholder(
                t(
                    lang,
                    "メッセージを入力…  (/ でコマンド)",
                    "Message the agent…  (/ for commands)",
                ),
                window,
                cx,
            )
        });
        set_input(
            &self.rename_input,
            t(lang, "セッション名", "Session name"),
            window,
            cx,
        );
        set_input(
            &self.file_filter_input,
            t(lang, "ファイルを検索…", "Search files…"),
            window,
            cx,
        );
        set_input(
            &self.new_project_name,
            t(lang, "表示名 (省略可)", "Name (optional)"),
            window,
            cx,
        );
        set_input(
            &self.new_project_path,
            t(lang, "フォルダのパス", "Folder path"),
            window,
            cx,
        );
        set_input(
            &self.new_quest_title,
            t(lang, "クエスト名", "Quest title"),
            window,
            cx,
        );
        set_input(
            &self.new_quest_goal,
            t(
                lang,
                "ゴール (省略可・プロンプトに付与)",
                "Goal (optional, sent with prompts)",
            ),
            window,
            cx,
        );
        set_input(&self.edit_provider_cmd, "e.g. opencode", window, cx);
        set_input(&self.edit_provider_args, "e.g. acp", window, cx);
        set_input(
            &self.edit_provider_model,
            t(lang, "未指定", "None"),
            window,
            cx,
        );
        set_input(&self.custom_id, "my-agent", window, cx);
        set_input(&self.custom_name, "My Agent", window, cx);
        set_input(&self.custom_cmd, "/path/to/agent", window, cx);
        set_input(&self.custom_args, "acp --stdio", window, cx);
        set_input(
            &self.custom_desc,
            t(lang, "このエージェントの説明", "What this agent is for"),
            window,
            cx,
        );
        set_input(
            &self.font_search,
            t(lang, "フォントを検索…", "Search fonts…"),
            window,
            cx,
        );
    }

    fn note(&mut self, kind: NoteKind, msg: impl Into<String>) {
        self.pending_notes.push((kind, msg.into()));
    }

    fn current_project(&self) -> Option<&Project> {
        let id = self.selected_project_id.as_deref()?;
        self.projects.iter().find(|p| p.id == id)
    }

    fn current_session(&self) -> Option<&Session> {
        let id = self.selected_session_id.as_deref()?;
        self.sessions.iter().find(|s| s.id == id)
    }

    fn provider_name(&self, id: &str) -> String {
        self.config
            .find_provider(id)
            .map(|p| p.name.clone())
            .unwrap_or_else(|| id.to_string())
    }

    fn session_title(&self, s: &Session) -> String {
        if s.title == DEFAULT_SESSION_TITLE {
            t(self.config.language, "新しいセッション", "New Session").to_string()
        } else {
            s.title.clone()
        }
    }

    fn is_session_running(&self, session_id: &str) -> bool {
        self.live_drafts.contains_key(session_id)
    }

    /// Jumps the feed to the latest message and keeps following new output
    /// (until the user scrolls up; scrolling back down resumes following).
    fn scroll_feed_to_end(&self) {
        self.feed.set_follow_mode(FollowMode::Tail);
    }

    /// Brings the feed's item count in line with the current session. Called
    /// on every render; cheap when nothing changed.
    fn sync_feed(&self) {
        let count = self.messages.len() + 1;
        let mut synced = self.feed_synced.borrow_mut();
        let session_changed = synced.0 != self.selected_session_id;
        let old = synced.1;
        if session_changed || count < old || old == 0 {
            self.feed.reset(count);
            self.scroll_feed_to_end();
        } else if count > old {
            // New messages: replace the old trailing item and append.
            self.feed.splice(old - 1..old, count - old + 1);
        }
        // The trailing item (live reply, permissions) changes while streaming.
        self.feed.remeasure_items(count - 1..count);
        *synced = (self.selected_session_id.clone(), count);
    }

    /// Forces a full feed rebuild on the next render (messages replaced).
    fn invalidate_feed(&self) {
        *self.feed_synced.borrow_mut() = (None, 0);
    }

    fn select_session(&mut self, session_id: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_session_id.as_deref() != Some(session_id.as_str()) {
            self.commit_rename(cx);
        }
        self.selected_session_id = Some(session_id);
        self.active_view = MainView::Chat;
        self.confirm_delete = None;
        self.reload_messages(window, cx);
        self.scroll_feed_to_end();
        self.prompt_input.update(cx, |s, cx| s.focus(window, cx));
    }

    fn create_new_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pid) = self.selected_project_id.clone() else {
            self.note(
                NoteKind::Warning,
                t(
                    self.config.language,
                    "先にプロジェクトを追加してください",
                    "Add a project first",
                ),
            );
            return;
        };
        // Reuse the current session if it is still untouched.
        if let Some(cur) = self.current_session() {
            if cur.title == DEFAULT_SESSION_TITLE
                && self.messages.is_empty()
                && cur.quest_id.as_deref() == self.selected_quest_filter.as_deref()
            {
                self.active_view = MainView::Chat;
                self.prompt_input.update(cx, |s, cx| s.focus(window, cx));
                return;
            }
        }
        // A new session keeps the agent, model and effort picked in the
        // current one.
        let (provider_id, model, effort) = self
            .current_session()
            .map(|s| (s.provider_id.clone(), s.model.clone(), s.effort.clone()))
            .unwrap_or_else(|| (self.config.default_provider_id.clone(), None, None));
        match self.storage.create_session(
            &pid,
            self.selected_quest_filter.as_deref(),
            DEFAULT_SESSION_TITLE,
            &provider_id,
            model.as_deref(),
        ) {
            Ok(mut sess) => {
                if effort.is_some() {
                    let _ = self.storage.set_session_effort(&sess.id, effort.as_deref());
                    sess.effort = effort;
                }
                let id = sess.id.clone();
                self.sessions.insert(0, sess);
                self.select_session(id, window, cx);
            }
            Err(e) => self.note(NoteKind::Error, format!("{e:#}")),
        }
    }

    fn reload_project_state(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project_id) = self.selected_project_id.clone() else {
            self.quests.clear();
            self.sessions.clear();
            self.selected_session_id = None;
            self.messages.clear();
            self.file_entries.clear();
            self.git_info = GitInfo::default();
            return;
        };

        self.quests = self.storage.list_quests(&project_id).unwrap_or_default();
        self.sessions = self.storage.list_sessions(&project_id).unwrap_or_default();

        if self.sessions.is_empty() {
            if let Ok(sess) = self.storage.create_session(
                &project_id,
                None,
                DEFAULT_SESSION_TITLE,
                &self.config.default_provider_id,
                None,
            ) {
                self.sessions.push(sess);
            }
        }

        let keep_selected = self
            .selected_session_id
            .as_deref()
            .is_some_and(|id| self.sessions.iter().any(|s| s.id == id));
        if !keep_selected {
            self.selected_session_id = self.sessions.first().map(|s| s.id.clone());
        }

        self.expanded_dirs.clear();
        self.reload_messages(window, cx);
        self.reload_workspace_files(cx);
        self.scroll_feed_to_end();
    }

    fn reload_messages(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        if let Some(sid) = self.selected_session_id.as_deref() {
            self.messages = self.storage.list_messages(sid).unwrap_or_default();
            self.invalidate_feed();
        } else {
            self.messages.clear();
        }
    }

    fn reload_workspace_files(&mut self, cx: &Context<Self>) {
        let filter = self.file_filter_input.read(cx).value().to_string();
        if let Some(proj) = self.current_project().cloned() {
            let root = PathBuf::from(&proj.workdir);
            self.file_entries = scan_directory(&root, &filter, &self.expanded_dirs, 2000);
            self.git_info = load_git_info(&root);
        } else {
            self.file_entries.clear();
            self.git_info = GitInfo::default();
        }
        if !self.git_info.is_repo && self.right_tab == RightTab::Git {
            self.right_tab = RightTab::Files;
        }
    }

    /// Models to offer for a session: what the agent reported, else the
    /// provider's configured list.
    fn model_choices(&self, provider: &ProviderConfig) -> Vec<ModelInfo> {
        if let Some(found) = self.agent_models.get(&provider.id) {
            if !found.is_empty() {
                return found.clone();
            }
        }
        provider
            .models
            .iter()
            .map(|m| ModelInfo {
                id: m.clone(),
                name: m.clone(),
                ..Default::default()
            })
            .collect()
    }

    /// Short label for the model a session uses, for the composer button.
    fn model_label(&self, session: &Session) -> String {
        let lang = self.config.language;
        let provider = self.config.find_provider(&session.provider_id);
        // With "Default" picked, show what Default means (ACP agents are
        // switched back to it on the next prompt).
        let id = session
            .model
            .clone()
            .or_else(|| provider.and_then(|p| p.default_model.clone()))
            .or_else(|| self.agent_default_model.get(&session.provider_id).cloned())
            .or_else(|| self.agent_current_model.get(&session.id).cloned());
        let Some(id) = id else {
            return t(lang, "既定モデル", "Default model").to_string();
        };
        self.agent_models
            .get(&session.provider_id)
            .and_then(|ms| ms.iter().find(|m| m.id == id))
            .map(|m| m.name.clone())
            .unwrap_or(id)
    }

    /// Handles `/model [name]` typed in the prompt. Returns false for any
    /// other text, which is then sent to the agent as usual.
    fn try_model_command(
        &mut self,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let lang = self.config.language;
        let Some(rest) = text.strip_prefix("/model") else {
            return false;
        };
        if !(rest.is_empty() || rest.starts_with(char::is_whitespace)) {
            return false;
        }
        let Some(session) = self.current_session().cloned() else {
            return true;
        };
        let arg = rest.trim();
        if arg.is_empty() {
            // Leave "/model " in the box so the suggestion list stays open.
            self.prompt_input.update(cx, |ta, cx| {
                ta.set_value("/model ", window, cx);
                ta.focus(window, cx);
            });
            self.prompt_empty = false;
            self.prompt_is_command = true;
            return true;
        }
        let model = if matches!(arg, "default" | "既定") {
            None
        } else {
            // Accept a display name too, e.g. "/model Claude Sonnet".
            let choices = self
                .config
                .find_provider(&session.provider_id)
                .map(|p| self.model_choices(p))
                .unwrap_or_default();
            let id = choices
                .iter()
                .find(|m| m.id == arg)
                .or_else(|| choices.iter().find(|m| m.name.eq_ignore_ascii_case(arg)))
                .map(|m| m.id.clone())
                .unwrap_or_else(|| arg.to_string());
            Some(id)
        };
        self.set_session_model(model);
        let label = self
            .current_session()
            .map(|s| self.model_label(s))
            .unwrap_or_default();
        self.note(
            NoteKind::Info,
            format!(
                "{}: {label}",
                t(lang, "モデルを変更しました", "Model set to")
            ),
        );
        self.prompt_input
            .update(cx, |ta, cx| ta.set_value("", window, cx));
        self.prompt_empty = true;
        self.prompt_is_command = false;
        true
    }

    fn set_session_model(&mut self, model: Option<String>) {
        let Some(sid) = self.selected_session_id.clone() else {
            return;
        };
        let Some(s) = self.sessions.iter_mut().find(|s| s.id == sid) else {
            return;
        };
        if s.model == model {
            return;
        }
        s.model = model.clone();
        let _ =
            self.storage
                .update_session_meta(&sid, None, None, Some(model.as_deref()), None, None);
        // Keep the effort only if the new model accepts it.
        let keep = self
            .current_session()
            .map(|sess| match sess.effort.as_deref() {
                Some(e) => self.effort_choices(sess).0.iter().any(|x| x == e),
                None => true,
            })
            .unwrap_or(true);
        if !keep {
            self.set_session_effort(None);
        }
    }

    fn set_session_effort(&mut self, effort: Option<String>) {
        let Some(sid) = self.selected_session_id.clone() else {
            return;
        };
        let Some(s) = self.sessions.iter_mut().find(|s| s.id == sid) else {
            return;
        };
        if s.effort == effort {
            return;
        }
        s.effort = effort.clone();
        let _ = self.storage.set_session_effort(&sid, effort.as_deref());
    }

    /// The effort levels offered for the session's model, and the one the
    /// agent uses when none is picked. Without a known model, every level any
    /// of the provider's models accepts.
    fn effort_choices(&self, session: &Session) -> (Vec<String>, Option<String>) {
        let Some(provider) = self.config.find_provider(&session.provider_id) else {
            return (Vec::new(), None);
        };
        let choices = self.model_choices(provider);
        let model_id = session
            .model
            .clone()
            .or_else(|| provider.default_model.clone())
            .or_else(|| self.agent_default_model.get(&session.provider_id).cloned())
            .or_else(|| self.agent_current_model.get(&session.id).cloned());
        if let Some(m) = model_id.and_then(|id| choices.iter().find(|m| m.id == id)) {
            return (m.efforts.clone(), m.default_effort.clone());
        }
        let mut all: Vec<String> = Vec::new();
        for e in choices.iter().flat_map(|m| m.efforts.iter()) {
            if !all.contains(e) {
                all.push(e.clone());
            }
        }
        (all, None)
    }

    fn start_rename(&mut self, session_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(s) = self.sessions.iter().find(|s| s.id == session_id) else {
            return;
        };
        let title = self.session_title(s);
        // The rename box lives in the sidebar row; make sure it is listed.
        if self.selected_quest_filter.is_some() && s.quest_id != self.selected_quest_filter {
            self.selected_quest_filter = None;
        }
        self.renaming_session_id = Some(session_id.to_string());
        self.rename_input.update(cx, |i, cx| {
            i.set_value(title, window, cx);
            i.focus(window, cx);
        });
    }

    fn commit_rename(&mut self, cx: &mut Context<Self>) {
        let Some(sid) = self.renaming_session_id.take() else {
            return;
        };
        let title = self.rename_input.read(cx).value().trim().to_string();
        if title.is_empty() {
            return;
        }
        let _ = self
            .storage
            .update_session_meta(&sid, Some(&title), None, None, None, None);
        if let Some(s) = self.sessions.iter_mut().find(|s| s.id == sid) {
            s.title = title;
        }
    }

    /// Returns true when the delete was confirmed (second click).
    fn confirm_or_arm_delete(&mut self, key: String, cx: &mut Context<Self>) -> bool {
        if self.confirm_delete.as_deref() == Some(key.as_str()) {
            self.confirm_delete = None;
            return true;
        }
        self.confirm_delete = Some(key.clone());
        // Disarm after a few seconds so a stray later click can't delete.
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(4)).await;
            let _ = this.update(cx, |this, cx| {
                if this.confirm_delete.as_deref() == Some(key.as_str()) {
                    this.confirm_delete = None;
                    cx.notify();
                }
            });
        })
        .detach();
        false
    }

    fn delete_session(&mut self, sid: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_session_running(&sid) {
            let mgr = self.agent_manager.clone();
            let s = sid.clone();
            self.rt.spawn(async move { mgr.cancel_session(&s).await });
            self.live_drafts.remove(&sid);
        }
        self.pending_permissions.retain(|p| p.session_id != sid);
        let _ = self.storage.delete_session(&sid);
        self.sessions.retain(|x| x.id != sid);
        if self.selected_session_id.as_deref() == Some(sid.as_str()) {
            self.selected_session_id = self.sessions.first().map(|x| x.id.clone());
            if self.selected_session_id.is_none() {
                // Always keep a session to type into.
                self.reload_project_state(window, cx);
            } else {
                self.reload_messages(window, cx);
            }
        }
    }

    fn select_project(&mut self, pid: String, window: &mut Window, cx: &mut Context<Self>) {
        self.selected_project_id = Some(pid);
        self.selected_quest_filter = None;
        self.selected_session_id = None;
        self.opened_file_path = None;
        self.file_dirty = false;
        self.active_view = MainView::Chat;
        self.confirm_delete = None;
        self.file_filter_input
            .update(cx, |i, cx| i.set_value("", window, cx));
        self.reload_project_state(window, cx);
        // Another project may run its agents somewhere else (e.g. WSL).
        self.refresh_provider_checks();
    }

    fn create_project_from_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let lang = self.config.language;
        let raw = self.new_project_path.read(cx).value().trim().to_string();
        if raw.is_empty() {
            self.note(
                NoteKind::Warning,
                t(
                    lang,
                    "フォルダのパスを入力してください",
                    "Enter a folder path",
                ),
            );
            return;
        }
        let expanded = expand_home(&raw);
        // Canonicalize so the same folder is never added twice, then drop
        // Windows' `\\?\` prefix, which child processes reject as a cwd.
        let path = std::fs::canonicalize(&expanded)
            .map(|p| PathBuf::from(simplify_path(&p.to_string_lossy())))
            .unwrap_or(expanded);
        if !path.is_dir() {
            self.note(
                NoteKind::Error,
                format!(
                    "{}: {}",
                    t(lang, "フォルダが見つかりません", "Folder not found"),
                    path.display()
                ),
            );
            return;
        }
        let path_str = path.to_string_lossy().to_string();
        if let Some(existing) = self.projects.iter().find(|p| p.workdir == path_str) {
            let id = existing.id.clone();
            self.show_new_project_form = false;
            self.select_project(id, window, cx);
            return;
        }
        let name_raw = self.new_project_name.read(cx).value().trim().to_string();
        let name = if name_raw.is_empty() {
            path.file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "Project".to_string())
        } else {
            name_raw
        };
        match self
            .storage
            .create_project(&self.workspace.id, &name, &path_str, "")
        {
            Ok(p) => {
                self.projects = self
                    .storage
                    .list_projects(&self.workspace.id)
                    .unwrap_or_default();
                self.show_new_project_form = false;
                self.new_project_name
                    .update(cx, |i, cx| i.set_value("", window, cx));
                self.select_project(p.id, window, cx);
            }
            Err(e) => self.note(NoteKind::Error, format!("{e:#}")),
        }
    }

    fn browse_project_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let lang = self.config.language;
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(t(lang, "選択", "Select").into()),
        });
        let path_input = self.new_project_path.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = rx.await;
            let _ = this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(Ok(Some(paths))) => {
                        if let Some(p) = paths.into_iter().next() {
                            path_input.update(cx, |i, cx| {
                                i.set_value(p.to_string_lossy().to_string(), window, cx)
                            });
                        }
                    }
                    Ok(Ok(None)) => {}
                    _ => this.note(
                        NoteKind::Warning,
                        t(
                            lang,
                            "フォルダ選択ダイアログを開けませんでした。パスを直接入力してください",
                            "Could not open the folder picker. Type the path instead",
                        ),
                    ),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn create_quest_from_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let title = self.new_quest_title.read(cx).value().trim().to_string();
        let goal = self.new_quest_goal.read(cx).value().trim().to_string();
        if title.is_empty() {
            self.note(
                NoteKind::Warning,
                t(
                    self.config.language,
                    "クエスト名を入力してください",
                    "Enter a quest title",
                ),
            );
            return;
        }
        let Some(pid) = self.selected_project_id.clone() else {
            return;
        };
        match self.storage.create_quest(&pid, &title, &goal) {
            Ok(q) => {
                self.selected_quest_filter = Some(q.id.clone());
                self.quests.insert(0, q);
                self.show_new_quest_form = false;
                self.new_quest_title
                    .update(cx, |i, cx| i.set_value("", window, cx));
                self.new_quest_goal
                    .update(cx, |i, cx| i.set_value("", window, cx));
                self.create_new_session(window, cx);
            }
            Err(e) => self.note(NoteKind::Error, format!("{e:#}")),
        }
    }

    fn open_file(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let lang = self.config.language;
        if self.file_dirty && self.opened_file_path.as_ref() != Some(&path) {
            self.note(
                NoteKind::Warning,
                t(
                    lang,
                    "未保存の変更を破棄しました",
                    "Discarded unsaved changes",
                ),
            );
        }
        let too_big = std::fs::metadata(&path)
            .map(|m| m.len() > MAX_OPEN_FILE_BYTES)
            .unwrap_or(false);
        if too_big {
            self.note(
                NoteKind::Warning,
                t(
                    lang,
                    "ファイルが大きすぎて開けません (2MB 超)",
                    "File too large to open (> 2 MB)",
                ),
            );
            return;
        }
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                self.opened_file_path = Some(path);
                self.opened_file_saved = content.clone();
                self.file_dirty = false;
                self.file_editor_input.update(cx, |ta, cx| {
                    ta.set_value(content, window, cx);
                });
            }
            Err(_) => self.note(
                NoteKind::Warning,
                t(
                    lang,
                    "テキストファイルとして開けません",
                    "Cannot open as a text file",
                ),
            ),
        }
    }

    /// Picks up changes made on disk (e.g. by an agent) to the open file.
    /// Unsaved edits are never overwritten; the user is told instead.
    fn reload_open_file_from_disk(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.opened_file_path.clone() else {
            return;
        };
        let Ok(disk) = std::fs::read_to_string(&path) else {
            return;
        };
        if disk == self.opened_file_saved {
            return;
        }
        let lang = self.config.language;
        if self.file_dirty {
            self.note(
                NoteKind::Warning,
                t(
                    lang,
                    "開いているファイルがディスク上で変更されました。保存すると上書きします",
                    "The open file changed on disk. Saving will overwrite it",
                ),
            );
            // Remember the disk version so the warning is shown once.
            self.opened_file_saved = disk;
            return;
        }
        self.opened_file_saved = disk.clone();
        self.file_editor_input
            .update(cx, |ta, cx| ta.set_value(disk, window, cx));
    }

    fn save_open_file(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.opened_file_path.clone() else {
            return;
        };
        let lang = self.config.language;
        let content = self.file_editor_input.read(cx).value().to_string();
        match std::fs::write(&path, &content) {
            Ok(()) => {
                self.opened_file_saved = content;
                self.file_dirty = false;
                self.reload_workspace_files(cx);
                self.note(
                    NoteKind::Success,
                    format!(
                        "{}: {}",
                        t(lang, "保存しました", "Saved"),
                        path.file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default()
                    ),
                );
            }
            Err(e) => self.note(
                NoteKind::Error,
                format!("{}: {e}", t(lang, "保存に失敗しました", "Save failed")),
            ),
        }
    }

    fn insert_file_reference(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let Some(proj) = self.current_project() else {
            return;
        };
        let rel = path
            .strip_prefix(&proj.workdir)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        let mut cur = self.prompt_input.read(cx).value().to_string();
        if !cur.is_empty() && !cur.ends_with(' ') && !cur.ends_with('\n') {
            cur.push(' ');
        }
        cur.push_str(&format!("`{rel}` "));
        self.prompt_input.update(cx, |ta, cx| {
            ta.set_value(cur, window, cx);
            ta.focus(window, cx);
        });
        self.prompt_empty = false;
    }

    /// The project's folder, or a neutral directory without a project.
    fn exec_cwd(&self) -> PathBuf {
        self.current_project()
            .map(|p| PathBuf::from(&p.workdir))
            .unwrap_or_else(std::env::temp_dir)
    }

    fn exec_mode(&self) -> ExecMode {
        self.current_project()
            .map(|p| self.config.exec_mode_for(&p.workdir))
            .unwrap_or_default()
    }

    fn exec_pref(&self) -> ExecPreference {
        match self.exec_mode() {
            ExecMode::Auto => ExecPreference::Auto,
            ExecMode::Host => ExecPreference::Host,
            ExecMode::Wsl => ExecPreference::Wsl,
        }
    }

    /// Where agents for the current project will run, for display.
    fn exec_label(&self) -> String {
        match agent_exec::predicted_target(self.exec_pref(), &self.exec_cwd()) {
            Some(target) => target.label(),
            None => "WSL".to_string(),
        }
    }

    /// Asks `provider`'s agent for its models in the background.
    fn start_model_discovery(&mut self, provider: ProviderConfig, manual: bool) {
        if !self.models_discovering.insert(provider.id.clone()) {
            return;
        }
        let manager = self.agent_manager.clone();
        let tx = self.ui_tx.clone();
        let pref = self.exec_pref();
        let cwd = self.exec_cwd();
        self.rt.spawn(async move {
            let (target, res) = manager.discover_models(&provider, pref, &cwd).await;
            let _ = tx.send(UiAsyncMsg::ModelsDiscovered {
                provider_id: provider.id,
                target,
                result: res.map_err(|e| format!("{e:#}")),
                manual,
            });
        });
    }

    fn save_models_cache(&self) {
        let path = self.paths.data_dir.join(MODELS_CACHE_FILE);
        match serde_json::to_string_pretty(&self.agent_models) {
            Ok(text) => {
                if let Err(e) = std::fs::write(&path, text) {
                    tracing::warn!("Failed to save {}: {e}", path.display());
                }
            }
            Err(e) => tracing::warn!("Failed to encode model cache: {e}"),
        }
    }

    fn refresh_provider_checks(&mut self) {
        self.check_generation += 1;
        self.provider_checks.clear();
        self.connect_results.clear();
        let generation = self.check_generation;
        let pref = self.exec_pref();
        let cwd = self.exec_cwd();
        for provider in self.config.providers.clone() {
            let manager = self.agent_manager.clone();
            let tx = self.ui_tx.clone();
            let cwd = cwd.clone();
            self.rt.spawn(async move {
                let res = manager.check_provider(&provider, pref, &cwd).await;
                let _ = tx.send(UiAsyncMsg::ProviderCheck(generation, res));
            });
        }
    }

    /// Changes where the current project's agents run.
    fn set_exec_mode(&mut self, mode: ExecMode) {
        let Some(proj) = self.current_project().cloned() else {
            return;
        };
        if self.config.exec_mode_for(&proj.workdir) == mode {
            return;
        }
        self.config.set_exec_mode_for(&proj.workdir, mode);
        self.save_config();
        // Agent-side sessions live in one environment; they can't be resumed
        // from the other one.
        for s in self.sessions.iter_mut() {
            if s.external_session_id.take().is_some() {
                let _ = self
                    .storage
                    .update_session_meta(&s.id, None, None, None, Some(None), None);
            }
        }
        self.refresh_provider_checks();
    }

    fn test_provider_connection(&self, provider: ProviderConfig) {
        let manager = self.agent_manager.clone();
        let tx = self.ui_tx.clone();
        let cwd = self.exec_cwd();
        let pref = self.exec_pref();
        let generation = self.check_generation;

        self.rt.spawn(async move {
            let check = manager.check_provider(&provider, pref, &cwd).await;
            let pid = provider.id.clone();
            let _ = tx.send(UiAsyncMsg::ProviderCheck(generation, check.clone()));

            if provider.kind == ProviderKind::Acp {
                match manager.test_acp_connection(&provider, pref, cwd).await {
                    Ok(info) => {
                        let _ = tx.send(UiAsyncMsg::ConnectResult {
                            provider_id: pid,
                            ok: true,
                            message: format!("ACP Connected: {info}"),
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(UiAsyncMsg::ConnectResult {
                            provider_id: pid,
                            ok: false,
                            message: format!("ACP Error: {e:#}"),
                        });
                    }
                }
            } else {
                let _ = tx.send(UiAsyncMsg::ConnectResult {
                    provider_id: pid,
                    ok: check.available,
                    message: if check.available {
                        format!("CLI Ready: {}", check.detail)
                    } else {
                        check.detail
                    },
                });
            }
        });
    }

    /// The command a setup step will run where this project's agents run.
    fn setup_command_preview(&self, provider: &ProviderConfig, action: ProviderSetup) -> String {
        let script = match action {
            ProviderSetup::InstallCli => match AgentManager::install_script(provider) {
                Some(s) => s,
                None => return String::new(),
            },
            ProviderSetup::Login => AgentManager::login_script(provider),
        };
        let native_windows = cfg!(windows) && !self.exec_label().starts_with("WSL");
        if native_windows {
            script.windows
        } else {
            script.unix
        }
    }

    fn run_provider_setup(&mut self, provider: ProviderConfig, action: ProviderSetup) {
        let lang = self.config.language;
        let preview = self.setup_command_preview(&provider, action);
        let opening = if cfg!(windows) {
            match lang {
                Language::Ja => format!(
                    "{} のターミナルを開きました。画面の指示に従ってください:\n$ {preview}",
                    self.exec_label()
                ),
                Language::En => format!(
                    "Opened a terminal ({}). Follow the prompts there:\n$ {preview}",
                    self.exec_label()
                ),
            }
        } else {
            format!("$ {preview}")
        };
        self.login_results.insert(provider.id.clone(), opening);
        self.setup_running.insert(provider.id.clone());

        let manager = self.agent_manager.clone();
        let tx = self.ui_tx.clone();
        let cwd = self.exec_cwd();
        let pref = self.exec_pref();
        self.rt.spawn(async move {
            let result = manager
                .run_provider_setup(&provider, action, pref, &cwd)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(UiAsyncMsg::SetupDone {
                provider_id: provider.id.clone(),
                result,
            });
        });
    }

    fn handle_async_msg(&mut self, msg: UiAsyncMsg) {
        match msg {
            UiAsyncMsg::ProviderCheck(generation, res) => {
                if generation == self.check_generation {
                    // Once per provider and environment per run, ask a
                    // working agent for its real model list.
                    let key = (res.provider_id.clone(), res.target.clone());
                    if res.available && !self.models_discovered.contains(&key) {
                        if let Some(p) = self.config.find_provider(&res.provider_id).cloned() {
                            self.start_model_discovery(p, false);
                        }
                    }
                    self.provider_checks.insert(res.provider_id.clone(), res);
                }
            }
            UiAsyncMsg::ModelsDiscovered {
                provider_id,
                target,
                result,
                manual,
            } => {
                self.models_discovering.remove(&provider_id);
                self.models_discovered
                    .insert((provider_id.clone(), target.clone()));
                let name = self.provider_name(&provider_id);
                if let Ok(found) = &result {
                    match &found.default_model {
                        Some(d) => {
                            self.agent_default_model
                                .insert(provider_id.clone(), d.clone());
                        }
                        None => {
                            self.agent_default_model.remove(&provider_id);
                        }
                    }
                }
                match result.map(|found| found.models) {
                    Ok(models) if !models.is_empty() => {
                        tracing::info!("{provider_id}: {} models ({target})", models.len());
                        if manual {
                            self.note(
                                NoteKind::Success,
                                match self.config.language {
                                    Language::Ja => {
                                        format!("{name}: モデル {} 件を取得しました", models.len())
                                    }
                                    Language::En => {
                                        format!("{name}: found {} models", models.len())
                                    }
                                },
                            );
                        }
                        self.agent_models.insert(provider_id, models);
                        self.save_models_cache();
                    }
                    other => {
                        let err = match other {
                            Err(e) => e,
                            Ok(_) => "no models reported".to_string(),
                        };
                        tracing::warn!("{provider_id}: model discovery failed ({target}): {err}");
                        let stamp = Local::now().format("%H:%M:%S");
                        self.logs.push(format!(
                            "{stamp} [{provider_id}] Model list unavailable ({target}): {err}"
                        ));
                        if manual {
                            self.note(
                                NoteKind::Warning,
                                match self.config.language {
                                    Language::Ja => {
                                        format!("{name}: モデル一覧を取得できませんでした: {err}")
                                    }
                                    Language::En => {
                                        format!("{name}: could not get the model list: {err}")
                                    }
                                },
                            );
                        }
                    }
                }
            }
            UiAsyncMsg::ConnectResult {
                provider_id,
                ok,
                message,
            } => {
                self.connect_results.insert(provider_id, (ok, message));
            }
            UiAsyncMsg::SetupDone {
                provider_id,
                result,
            } => {
                self.setup_running.remove(&provider_id);
                match result {
                    Ok(outcome) => {
                        if let Some(command) = outcome.installed_command {
                            if let Some(p) = self
                                .config
                                .providers
                                .iter_mut()
                                .find(|p| p.id == provider_id)
                            {
                                p.command = command;
                            }
                            self.save_config();
                        }
                        self.login_results.insert(provider_id, outcome.message);
                    }
                    Err(e) => {
                        self.login_results
                            .insert(provider_id, format!("Error: {e}"));
                    }
                }
                // Whatever was installed or signed in to, show the new state.
                self.refresh_provider_checks();
            }
            UiAsyncMsg::Event(ev) => self.handle_same_event(ev),
        }
    }

    fn handle_same_event(&mut self, ev: SameEvent) {
        match ev {
            SameEvent::SessionStarted { session_id, .. } => {
                self.live_drafts.entry(session_id).or_default();
            }
            SameEvent::SessionStatusChanged { session_id, status } => {
                let _ = self.storage.update_session_meta(
                    &session_id,
                    None,
                    None,
                    None,
                    None,
                    Some(status),
                );
                if let Some(s) = self.sessions.iter_mut().find(|s| s.id == session_id) {
                    s.status = status;
                }
            }
            SameEvent::ExternalSessionBound {
                session_id,
                external_session_id,
            } => {
                let _ = self.storage.update_session_meta(
                    &session_id,
                    None,
                    None,
                    None,
                    Some(Some(&external_session_id)),
                    None,
                );
                if let Some(s) = self.sessions.iter_mut().find(|s| s.id == session_id) {
                    s.external_session_id = Some(external_session_id);
                }
            }
            SameEvent::MessageDelta { session_id, text } => {
                if let Some(d) = self.live_drafts.get_mut(&session_id) {
                    d.content.push_str(&text);
                }
            }
            SameEvent::ThinkingDelta { session_id, text } => {
                if let Some(d) = self.live_drafts.get_mut(&session_id) {
                    d.thinking.push_str(&text);
                }
            }
            SameEvent::ToolCallStarted {
                session_id,
                tool_call,
            } => {
                let Some(draft) = self.live_drafts.get_mut(&session_id) else {
                    return;
                };
                if let Some(existing) = draft.tool_calls.iter_mut().find(|t| t.id == tool_call.id) {
                    *existing = tool_call;
                } else {
                    draft.tool_calls.push(tool_call);
                }
            }
            SameEvent::ToolCallUpdated {
                session_id,
                tool_call,
            } => {
                let Some(draft) = self.live_drafts.get_mut(&session_id) else {
                    return;
                };
                if let Some(existing) = draft.tool_calls.iter_mut().find(|t| t.id == tool_call.id) {
                    if !tool_call.title.is_empty() {
                        existing.title = tool_call.title;
                    }
                    if tool_call.kind.is_some() {
                        existing.kind = tool_call.kind;
                    }
                    existing.status = tool_call.status;
                    if tool_call.input.is_some() {
                        existing.input = tool_call.input;
                    }
                    if tool_call.output.is_some() {
                        existing.output = tool_call.output;
                    }
                } else {
                    draft.tool_calls.push(tool_call);
                }
            }
            SameEvent::ApprovalRequested { request } => {
                // A request that arrives after the run was stopped can't be
                // acted on; decline it so the agent is not left waiting.
                if !self.live_drafts.contains_key(&request.session_id) {
                    let mgr = self.agent_manager.clone();
                    let rid = request.request_id.clone();
                    self.rt
                        .spawn(async move { mgr.respond_permission(&rid, None).await });
                    return;
                }
                if self.selected_session_id.as_deref() != Some(request.session_id.as_str()) {
                    let title = self
                        .sessions
                        .iter()
                        .find(|s| s.id == request.session_id)
                        .map(|s| self.session_title(s))
                        .unwrap_or_default();
                    self.note(
                        NoteKind::Warning,
                        format!(
                            "{}: {title}",
                            t(self.config.language, "承認待ち", "Approval needed")
                        ),
                    );
                }
                self.pending_permissions.push(request);
            }
            SameEvent::SlashCommandsUpdated {
                session_id,
                commands,
            } => {
                self.slash_commands.insert(session_id, commands);
            }
            SameEvent::ModelsUpdated {
                session_id,
                provider_id,
                models,
                current,
            } => {
                if !models.is_empty() && self.agent_models.get(&provider_id) != Some(&models) {
                    self.agent_models.insert(provider_id, models);
                    self.save_models_cache();
                }
                match current {
                    Some(m) => {
                        self.agent_current_model.insert(session_id, m);
                    }
                    None => {
                        self.agent_current_model.remove(&session_id);
                    }
                }
            }
            SameEvent::SessionCompleted { session_id, .. } => {
                self.finalize_live_draft(&session_id, SessionStatus::Completed, None);
            }
            SameEvent::SessionCancelled { session_id } => {
                self.finalize_live_draft(&session_id, SessionStatus::Cancelled, None);
            }
            SameEvent::SessionFailed { session_id, error } => {
                self.finalize_live_draft(&session_id, SessionStatus::Failed, Some(error));
            }
            SameEvent::AgentLog {
                provider_id, line, ..
            } => {
                let stamp = Local::now().format("%H:%M:%S");
                self.logs.push(format!("{stamp} [{provider_id}] {line}"));
                if self.logs.len() > 1000 {
                    let drain_n = self.logs.len() - 1000;
                    self.logs.drain(0..drain_n);
                }
            }
            SameEvent::ProviderCapabilitiesUpdated { .. } => {}
        }
    }

    fn finalize_live_draft(
        &mut self,
        session_id: &str,
        status: SessionStatus,
        error: Option<String>,
    ) {
        // A run can report both failure and a final status; only the first
        // one owns the draft.
        let Some(draft) = self.live_drafts.remove(session_id) else {
            return;
        };
        let lang = self.config.language;
        let sess = self.storage.get_session(session_id).ok().flatten();
        let mut content = draft.content;
        let mut tool_calls = draft.tool_calls;
        // Tools still pending when a run ends never finished.
        if status != SessionStatus::Completed {
            for tc in tool_calls.iter_mut() {
                if matches!(
                    tc.status,
                    ToolCallStatus::Pending | ToolCallStatus::InProgress
                ) {
                    tc.status = if status == SessionStatus::Cancelled {
                        ToolCallStatus::Denied
                    } else {
                        ToolCallStatus::Failed
                    };
                }
            }
        }
        if let Some(err) = &error {
            if !content.is_empty() {
                content.push_str("\n\n");
            }
            content.push_str(ERROR_MARKER);
            content.push_str(err);
        }
        if status == SessionStatus::Cancelled {
            if !content.trim().is_empty() {
                content.push_str("\n\n");
            }
            content.push_str(t(lang, "_(停止しました)_", "_(Stopped)_"));
        }
        if !content.is_empty() || !draft.thinking.is_empty() || !tool_calls.is_empty() {
            let msg = Message {
                id: Uuid::new_v4().to_string(),
                session_id: session_id.to_string(),
                role: MessageRole::Assistant,
                content,
                thinking: if draft.thinking.is_empty() {
                    None
                } else {
                    Some(draft.thinking)
                },
                tool_calls,
                provider_id: sess.as_ref().map(|s| s.provider_id.clone()),
                model: sess.as_ref().and_then(|s| s.model.clone()),
                created_at: Utc::now(),
            };
            let _ = self.storage.append_message(&msg);
            if self.selected_session_id.as_deref() == Some(session_id) {
                self.messages.push(msg);
            }
        }
        let _ = self
            .storage
            .update_session_meta(session_id, None, None, None, None, Some(status));
        if let Some(s) = self.sessions.iter_mut().find(|s| s.id == session_id) {
            s.status = status;
        }
        self.pending_permissions
            .retain(|p| p.session_id != session_id);

        let title = sess
            .as_ref()
            .map(|s| self.session_title(s))
            .unwrap_or_default();
        let visible = self.selected_session_id.as_deref() == Some(session_id);
        if let (Some(err), false) = (error, visible) {
            let first_line: String = err.lines().next().unwrap_or("").chars().take(160).collect();
            self.note(
                NoteKind::Error,
                format!(
                    "{}「{title}」: {first_line}",
                    t(lang, "エラー", "Error in ")
                ),
            );
        } else if status == SessionStatus::Completed
            && self.selected_session_id.as_deref() != Some(session_id)
        {
            self.note(
                NoteKind::Success,
                format!("{}: {title}", t(lang, "完了", "Finished")),
            );
        }

        if let Some(proj) = self.current_project().cloned() {
            self.git_info = load_git_info(&PathBuf::from(&proj.workdir));
            // The agent may have edited the file that is open in the panel.
            self.reload_open_file_pending = true;
        }
    }

    fn send_current_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let lang = self.config.language;
        let text = self.prompt_input.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        // Local commands never reach the agent (and work while it runs).
        if self.try_model_command(&text, window, cx) {
            return;
        }
        let Some(project) = self.current_project().cloned() else {
            self.note(
                NoteKind::Warning,
                t(
                    lang,
                    "先にプロジェクトを追加してください",
                    "Add a project first",
                ),
            );
            return;
        };
        let Some(session) = self.current_session().cloned() else {
            return;
        };
        if self.is_session_running(&session.id) {
            self.note(
                NoteKind::Info,
                t(
                    lang,
                    "応答中です。停止するか完了を待ってください",
                    "The agent is still responding. Stop it or wait for it to finish",
                ),
            );
            return;
        }
        let Some(provider) = self.config.find_provider(&session.provider_id).cloned() else {
            self.note(
                NoteKind::Error,
                format!(
                    "{}: {}",
                    t(lang, "プロバイダが見つかりません", "Provider not found"),
                    session.provider_id
                ),
            );
            return;
        };
        if !provider.enabled {
            self.note(
                NoteKind::Warning,
                format!(
                    "{}: {}",
                    t(lang, "無効化されたプロバイダです", "Provider is disabled"),
                    provider.name
                ),
            );
            return;
        }

        self.prompt_input.update(cx, |ta, cx| {
            ta.set_value("", window, cx);
        });
        self.prompt_empty = true;
        self.prompt_is_command = false;

        if session.title == DEFAULT_SESSION_TITLE {
            let first_line = text.lines().next().unwrap_or("");
            let mut new_title: String = first_line.chars().take(40).collect();
            if first_line.chars().count() > 40 {
                new_title.push('…');
            }
            let _ = self.storage.update_session_meta(
                &session.id,
                Some(&new_title),
                None,
                None,
                None,
                None,
            );
            if let Some(s) = self.sessions.iter_mut().find(|s| s.id == session.id) {
                s.title = new_title;
            }
        }

        let model = session
            .model
            .clone()
            .filter(|m| !m.trim().is_empty())
            .or_else(|| provider.default_model.clone());

        let user_msg = Message {
            id: Uuid::new_v4().to_string(),
            session_id: session.id.clone(),
            role: MessageRole::User,
            content: text.clone(),
            thinking: None,
            tool_calls: vec![],
            provider_id: Some(provider.id.clone()),
            model: model.clone(),
            created_at: Utc::now(),
        };
        let _ = self.storage.append_message(&user_msg);
        self.messages.push(user_msg);

        // Move the session to the top of the list, like most chat apps.
        if let Some(pos) = self.sessions.iter().position(|s| s.id == session.id) {
            let s = self.sessions.remove(pos);
            self.sessions.insert(0, s);
        }

        let mut full_prompt = text;
        if session.external_session_id.is_none() {
            if let Some(qid) = session.quest_id.as_deref() {
                if let Some(quest) = self.quests.iter().find(|q| q.id == qid) {
                    if !quest.goal.trim().is_empty() {
                        full_prompt = format!(
                            "[Quest: {}]\nGoal: {}\n\n{}",
                            quest.title, quest.goal, full_prompt
                        );
                    }
                }
            }
        }

        self.live_drafts
            .insert(session.id.clone(), LiveDraft::default());
        self.scroll_feed_to_end();
        self.agent_manager.start_prompt(PromptRequest {
            session_id: session.id,
            external_session_id: session.external_session_id,
            provider,
            model,
            effort: session.effort.clone().filter(|e| !e.trim().is_empty()),
            cwd: PathBuf::from(&project.workdir),
            prompt: full_prompt,
            exec: self.exec_pref(),
        });
    }

    fn cancel_current(&mut self) {
        let Some(sid) = self.selected_session_id.clone() else {
            return;
        };
        if !self.is_session_running(&sid) {
            return;
        }
        // Answer any open permission prompt first, or the agent stays
        // blocked on it and never sees the cancel.
        let perms: Vec<String> = self
            .pending_permissions
            .iter()
            .filter(|p| p.session_id == sid)
            .map(|p| p.request_id.clone())
            .collect();
        self.pending_permissions.retain(|p| p.session_id != sid);
        let mgr = self.agent_manager.clone();
        self.rt.spawn(async move {
            for rid in perms {
                mgr.respond_permission(&rid, None).await;
            }
            mgr.cancel_session(&sid).await;
        });
    }

    fn save_config(&mut self) {
        if let Err(err) = self.config.save(&self.paths) {
            self.note(NoteKind::Error, format!("Failed to save config: {err}"));
        }
    }

    fn open_settings(&mut self, section: SettingsSection) {
        self.active_view = MainView::Settings;
        self.settings_section = section;
        if section == SettingsSection::Providers {
            self.refresh_provider_checks();
        }
    }

    fn set_language(&mut self, lang: Language, window: &mut Window, cx: &mut Context<Self>) {
        if self.config.language == lang {
            return;
        }
        self.config.language = lang;
        self.save_config();
        self.apply_placeholders(window, cx);
        self.feed.remeasure();
    }

    fn set_theme_mode(&mut self, mode: ThemeMode, window: &mut Window, cx: &mut Context<Self>) {
        if self.config.theme == mode {
            return;
        }
        self.config.theme = mode;
        apply_gpui_theme(&self.config, Some(window), cx);
        self.save_config();
    }

    fn set_accent(&mut self, accent_color: AccentColor, window: &mut Window, cx: &mut Context<Self>) {
        if self.config.accent == accent_color {
            return;
        }
        self.config.accent = accent_color;
        apply_gpui_theme(&self.config, Some(window), cx);
        self.save_config();
    }

    /// Sets the UI (`mono == false`) or code font; `None` restores the
    /// platform default.
    fn set_font(
        &mut self,
        mono: bool,
        family: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let family = family
            .map(|f| f.trim().to_string())
            .filter(|f| !f.is_empty());
        let slot = if mono {
            &mut self.config.mono_font
        } else {
            &mut self.config.ui_font
        };
        if *slot == family {
            return;
        }
        *slot = family;
        apply_gpui_theme(&self.config, Some(window), cx);
        self.save_config();
        self.feed.remeasure();
        window.refresh();
    }

    /// Opens (or closes, if already open) the font dropdown of one picker:
    /// clears the search, focuses it and scrolls to the current choice.
    fn toggle_font_menu(&mut self, mono: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.font_menu_open == Some(mono) {
            self.font_menu_open = None;
            return;
        }
        self.font_menu_open = Some(mono);
        self.font_search.update(cx, |s, cx| {
            s.set_value("", window, cx);
            s.focus(window, cx);
        });
        self.filter_font_rows(cx);
        let current = if mono {
            &self.config.mono_font
        } else {
            &self.config.ui_font
        };
        let target = current
            .as_deref()
            .and_then(|c| self.font_names.iter().position(|n| n == c));
        if let Some(ix) = self.font_rows.iter().position(|r| *r == target) {
            self.font_list.scroll_to(gpui::ListOffset {
                item_ix: ix,
                offset_in_item: px(0.0),
            });
        }
    }

    /// Rebuilds `font_rows` from the search box: case-insensitive substring
    /// match; the "default" row only shows while the search is empty.
    fn filter_font_rows(&mut self, cx: &mut Context<Self>) {
        let query = self.font_search.read(cx).value().trim().to_lowercase();
        self.font_rows = if query.is_empty() {
            std::iter::once(None)
                .chain((0..self.font_names.len()).map(Some))
                .collect()
        } else {
            self.font_names
                .iter()
                .enumerate()
                .filter(|(_, n)| n.to_lowercase().contains(&query))
                .map(|(i, _)| Some(i))
                .collect()
        };
        self.font_list.reset(self.font_rows.len());
    }

    /// Sets the base font size in px; `None` restores the default.
    fn set_font_size(&mut self, size: Option<f32>, window: &mut Window, cx: &mut Context<Self>) {
        let size = size.map(|s| s.round().clamp(MIN_FONT_SIZE, MAX_FONT_SIZE));
        if self.config.font_size() == size {
            return;
        }
        self.config.font_size = size;
        apply_gpui_theme(&self.config, Some(window), cx);
        self.save_config();
        self.feed.remeasure();
        self.font_list.remeasure();
        window.refresh();
    }

    /// The chat prompt. With [`SubmitKey::Enter`] the textarea sends on
    /// Enter itself; otherwise Enter inserts a newline and
    /// [`Self::on_prompt_event`] sends on Ctrl/Cmd+Enter.
    fn new_prompt_input(
        submit_key: SubmitKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<TextareaState> {
        cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(2, 12)
                .submit_on_enter(submit_key == SubmitKey::Enter)
        })
    }

    fn on_prompt_event(
        &mut self,
        state: &Entity<TextareaState>,
        ev: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match ev {
            InputEvent::PressEnter { shift, .. } => {
                let send = match self.config.submit_key {
                    // Shift+Enter inserts a newline but still emits PressEnter.
                    SubmitKey::Enter => !*shift,
                    // Enter already inserted its newline; the prompt is
                    // trimmed before sending.
                    SubmitKey::ModEnter => window.modifiers().secondary(cx),
                };
                if send {
                    self.send_current_prompt(window, cx);
                    cx.notify();
                }
            }
            InputEvent::Change => {
                let value = state.read(cx).value();
                let empty = value.trim().is_empty();
                let is_command = value.starts_with('/') && !value.contains('\n');
                // Suggestions filter on every keystroke while a command is
                // typed, so notify for those too.
                if empty != self.prompt_empty
                    || is_command
                    || is_command != self.prompt_is_command
                {
                    self.prompt_empty = empty;
                    self.prompt_is_command = is_command;
                    cx.notify();
                }
            }
            _ => {}
        }
    }

    /// Switches the send key in place, preserving the draft's undo history,
    /// selection, and caret position.
    fn set_submit_key(&mut self, key: SubmitKey, window: &mut Window, cx: &mut Context<Self>) {
        if self.config.submit_key == key {
            return;
        }
        self.config.submit_key = key;
        self.save_config();
        self.prompt_input
            .update(cx, |ta, cx| ta.set_submit_on_enter(key == SubmitKey::Enter, cx));
        self.apply_placeholders(window, cx);
    }

    fn set_auto_approve(&mut self, value: bool) {
        if self.config.auto_approve_tools == value {
            return;
        }
        self.config.auto_approve_tools = value;
        self.agent_manager.set_auto_approve(value);
        self.save_config();
    }
}

fn expand_home(raw: &str) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
        if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(raw)
}

fn load_models_cache(data_dir: &Path) -> HashMap<String, Vec<ModelInfo>> {
    std::fs::read_to_string(data_dir.join(MODELS_CACHE_FILE))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn rel_time(dt: DateTime<Utc>, lang: Language) -> String {
    let secs = (Utc::now() - dt).num_seconds().max(0);
    match lang {
        Language::Ja => match secs {
            0..=59 => "たった今".to_string(),
            60..=3599 => format!("{}分前", secs / 60),
            3600..=86_399 => format!("{}時間前", secs / 3600),
            86_400..=604_799 => format!("{}日前", secs / 86_400),
            _ => dt.with_timezone(&Local).format("%m/%d").to_string(),
        },
        Language::En => match secs {
            0..=59 => "now".to_string(),
            60..=3599 => format!("{}m", secs / 60),
            3600..=86_399 => format!("{}h", secs / 3600),
            86_400..=604_799 => format!("{}d", secs / 86_400),
            _ => dt.with_timezone(&Local).format("%b %d").to_string(),
        },
    }
}

/// A left-aligned, full-width list row with hover and selected states.
fn list_row(id: impl Into<ElementId>, selected: bool, theme: &Theme) -> Stateful<Div> {
    div()
        .id(id)
        .h_flex()
        .items_center()
        .gap_2()
        .w_full()
        .px_2()
        .py(px(4.0))
        .rounded(px(6.0))
        .cursor_pointer()
        .text_sm()
        .when(selected, |d| d.bg(theme.list_active))
        .when(!selected, |d| d.hover(|s| s.bg(theme.list_hover)))
}

fn section_label(text: &str, theme: &Theme) -> Div {
    div()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme.muted_foreground)
        .child(text.to_string())
}

fn status_color(status: SessionStatus) -> Hsla {
    match status {
        SessionStatus::Running | SessionStatus::WaitingForSubagent => accent().into(),
        SessionStatus::WaitingForApproval => status_amber().into(),
        SessionStatus::Completed => status_green().into(),
        SessionStatus::Failed => status_red().into(),
        SessionStatus::Interrupted | SessionStatus::Cancelled => gpui::rgb(0xA0A0A0).into(),
        SessionStatus::Idle => gpui::rgb(0x666666).into(),
    }
}

/// Separates a failed run's error (stored after [`ERROR_MARKER`]) from the reply.
fn split_error(content: &str) -> (&str, Option<&str>) {
    match content.split_once(ERROR_MARKER) {
        Some((body, err)) => (body.trim_end(), Some(err)),
        None => (content, None),
    }
}

impl SameAppView {
    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let lang = self.config.language;

        div()
            .v_flex()
            .w(px(272.0))
            .h_full()
            .flex_shrink_0()
            .bg(theme.sidebar)
            .border_r_1()
            .border_color(theme.border)
            .p_3()
            .gap_2()
            // Primary navigation: Chat vs Providers Connection Screen
            .child(
                div()
                    .h_flex()
                    .gap_1()
                    .child(
                        Button::new("nav-chat")
                            .small()
                            .flex_1()
                            .when(self.active_view == MainView::Chat, |b| b.primary())
                            .when(self.active_view != MainView::Chat, |b| b.ghost())
                            .label(t(lang, "ワークスペース", "Workspace"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.active_view = MainView::Chat;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("nav-settings")
                            .small()
                            .flex_1()
                            .when(self.active_view == MainView::Settings, |b| b.primary())
                            .when(self.active_view != MainView::Settings, |b| b.ghost())
                            .label(t(lang, "設定", "Settings"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                let section = this.settings_section;
                                this.open_settings(section);
                                cx.notify();
                            })),
                    ),
            )
            .child(div().h(px(1.0)).bg(theme.border))
            .child(self.render_projects_section(cx))
            .child(div().h(px(1.0)).bg(theme.border))
            .child(self.render_quests_section(cx))
            .child(div().h(px(1.0)).bg(theme.border))
            .child(self.render_sessions_section(cx))
    }

    fn delete_button(
        &self,
        id: String,
        key: String,
        theme: &Theme,
        on_confirm: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let lang = self.config.language;
        let armed = self.confirm_delete.as_deref() == Some(key.as_str());
        div()
            .id(SharedString::from(id))
            .flex_shrink_0()
            .px_1()
            .rounded(px(4.0))
            .text_xs()
            .cursor_pointer()
            .when(armed, |d| {
                d.bg(theme.danger)
                    .text_color(theme.danger_foreground)
                    .child(t(lang, "削除?", "Delete?"))
            })
            .when(!armed, |d| {
                d.text_color(theme.muted_foreground)
                    .opacity(0.0)
                    .group_hover("row", |s| s.opacity(1.0))
                    .hover(|s| s.bg(theme.list_hover).text_color(theme.foreground))
                    .child("✕")
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                if this.confirm_or_arm_delete(key.clone(), cx) {
                    on_confirm(this, window, cx);
                }
                cx.notify();
            }))
            .into_any_element()
    }

    fn render_projects_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let lang = self.config.language;
        div()
            .v_flex()
            .gap_1()
            .child(
                div()
                    .h_flex()
                    .justify_between()
                    .items_center()
                    .child(section_label(t(lang, "プロジェクト", "PROJECTS"), &theme))
                    .child(
                        Button::new("btn-new-project")
                            .xsmall()
                            .ghost()
                            .tooltip(t(lang, "プロジェクトを追加", "Add project"))
                            .label("+")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.show_new_project_form = !this.show_new_project_form;
                                if this.show_new_project_form {
                                    let cwd = this
                                        .current_project()
                                        .and_then(|p| {
                                            PathBuf::from(&p.workdir)
                                                .parent()
                                                .map(|x| x.to_string_lossy().to_string())
                                        })
                                        .or_else(|| {
                                            std::env::current_dir()
                                                .ok()
                                                .map(|p| p.to_string_lossy().to_string())
                                        })
                                        .unwrap_or_default();
                                    this.new_project_path.update(cx, |i, cx| {
                                        i.set_value(cwd, window, cx);
                                        i.focus(window, cx);
                                    });
                                }
                                cx.notify();
                            })),
                    ),
            )
            // Inline New Project Form
            .when(self.show_new_project_form, |el| {
                el.child(
                    div()
                        .v_flex()
                        .gap_1()
                        .p_2()
                        .rounded(px(6.0))
                        .bg(theme.background)
                        .border_1()
                        .border_color(theme.border)
                        .child(
                            div()
                                .h_flex()
                                .gap_1()
                                .child(
                                    div()
                                        .flex_1()
                                        .child(Input::new(&self.new_project_path).xsmall()),
                                )
                                .child(
                                    Button::new("browse-project")
                                        .xsmall()
                                        .ghost()
                                        .tooltip(t(lang, "フォルダを選択", "Choose folder"))
                                        .label("…")
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.browse_project_folder(window, cx);
                                        })),
                                ),
                        )
                        .child(Input::new(&self.new_project_name).xsmall())
                        .child(
                            div()
                                .h_flex()
                                .gap_1()
                                .justify_end()
                                .child(
                                    Button::new("cancel-new-project")
                                        .xsmall()
                                        .ghost()
                                        .label(t(lang, "キャンセル", "Cancel"))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.show_new_project_form = false;
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    Button::new("confirm-new-project")
                                        .xsmall()
                                        .primary()
                                        .label(t(lang, "追加", "Add"))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.create_project_from_form(window, cx);
                                            cx.notify();
                                        })),
                                ),
                        ),
                )
            })
            .when(
                self.projects.is_empty() && !self.show_new_project_form,
                |el| {
                    el.child(div().text_xs().text_color(theme.muted_foreground).child(t(
                        lang,
                        "+ からフォルダを追加",
                        "Add a folder with +",
                    )))
                },
            )
            .child(
                div()
                    .id("projects-scroll")
                    .v_flex()
                    .max_h(px(160.0))
                    .overflow_y_scroll()
                    .children(self.projects.iter().map(|proj| {
                        let pid = proj.id.clone();
                        let pid_del = proj.id.clone();
                        let selected =
                            self.selected_project_id.as_deref() == Some(proj.id.as_str());
                        list_row(
                            SharedString::from(format!("proj-{}", proj.id)),
                            selected,
                            &theme,
                        )
                        .group("row")
                        .child(div().text_xs().child("📁"))
                        .child(
                            div()
                                .v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(div().truncate().child(proj.name.clone()))
                                .when(selected, |d| {
                                    d.child(
                                        div()
                                            .text_xs()
                                            .truncate()
                                            .text_color(theme.muted_foreground)
                                            .child(proj.workdir.clone()),
                                    )
                                }),
                        )
                        .child(self.delete_button(
                            format!("del-proj-{}", proj.id),
                            format!("proj:{}", proj.id),
                            &theme,
                            move |this, window, cx| {
                                let _ = this.storage.delete_project(&pid_del);
                                this.projects.retain(|p| p.id != pid_del);
                                if this.selected_project_id.as_deref() == Some(pid_del.as_str()) {
                                    match this.projects.first().map(|p| p.id.clone()) {
                                        Some(next) => this.select_project(next, window, cx),
                                        None => {
                                            this.selected_project_id = None;
                                            this.reload_project_state(window, cx);
                                        }
                                    }
                                }
                            },
                            cx,
                        ))
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                if this.selected_project_id.as_deref() != Some(pid.as_str()) {
                                    this.select_project(pid.clone(), window, cx);
                                } else {
                                    this.active_view = MainView::Chat;
                                }
                                cx.notify();
                            },
                        ))
                    })),
            )
    }

    fn render_quests_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let lang = self.config.language;
        div()
            .v_flex()
            .gap_1()
            .child(
                div()
                    .h_flex()
                    .justify_between()
                    .items_center()
                    .child(section_label(t(lang, "クエスト", "QUESTS"), &theme))
                    .child(
                        Button::new("btn-new-quest")
                            .xsmall()
                            .ghost()
                            .tooltip(t(lang, "クエストを作成", "New quest"))
                            .label("+")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.show_new_quest_form = !this.show_new_quest_form;
                                if this.show_new_quest_form {
                                    this.new_quest_title.update(cx, |i, cx| i.focus(window, cx));
                                }
                                cx.notify();
                            })),
                    ),
            )
            .when(self.show_new_quest_form, |el| {
                el.child(
                    div()
                        .v_flex()
                        .gap_1()
                        .p_2()
                        .rounded(px(6.0))
                        .bg(theme.background)
                        .border_1()
                        .border_color(theme.border)
                        .child(Input::new(&self.new_quest_title).xsmall())
                        .child(Input::new(&self.new_quest_goal).xsmall())
                        .child(
                            div()
                                .h_flex()
                                .gap_1()
                                .justify_end()
                                .child(
                                    Button::new("cancel-new-quest")
                                        .xsmall()
                                        .ghost()
                                        .label(t(lang, "キャンセル", "Cancel"))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.show_new_quest_form = false;
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    Button::new("confirm-new-quest")
                                        .xsmall()
                                        .primary()
                                        .label(t(lang, "作成", "Create"))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.create_quest_from_form(window, cx);
                                            cx.notify();
                                        })),
                                ),
                        ),
                )
            })
            .child(
                div()
                    .id("quests-scroll")
                    .v_flex()
                    .max_h(px(150.0))
                    .overflow_y_scroll()
                    .child(
                        list_row("quest-all", self.selected_quest_filter.is_none(), &theme)
                            .child(div().text_xs().child("☰"))
                            .child(div().flex_1().truncate().child(t(
                                lang,
                                "すべてのセッション",
                                "All sessions",
                            )))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(self.sessions.len().to_string()),
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.selected_quest_filter = None;
                                this.active_view = MainView::Chat;
                                cx.notify();
                            })),
                    )
                    .children(self.quests.iter().map(|q| {
                        let qid = q.id.clone();
                        let qid_del = q.id.clone();
                        let selected = self.selected_quest_filter.as_deref() == Some(q.id.as_str());
                        let count = self
                            .sessions
                            .iter()
                            .filter(|s| s.quest_id.as_deref() == Some(q.id.as_str()))
                            .count();
                        list_row(
                            SharedString::from(format!("quest-{}", q.id)),
                            selected,
                            &theme,
                        )
                        .group("row")
                        .child(div().text_xs().child("🎯"))
                        .child(div().flex_1().min_w_0().truncate().child(q.title.clone()))
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(count.to_string()),
                        )
                        .child(self.delete_button(
                            format!("del-quest-{}", q.id),
                            format!("quest:{}", q.id),
                            &theme,
                            move |this, _, _| {
                                let _ = this.storage.delete_quest(&qid_del);
                                this.quests.retain(|q| q.id != qid_del);
                                for s in this.sessions.iter_mut() {
                                    if s.quest_id.as_deref() == Some(qid_del.as_str()) {
                                        s.quest_id = None;
                                    }
                                }
                                if this.selected_quest_filter.as_deref() == Some(qid_del.as_str()) {
                                    this.selected_quest_filter = None;
                                }
                            },
                            cx,
                        ))
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.selected_quest_filter = Some(qid.clone());
                                this.active_view = MainView::Chat;
                                // Jump to the quest's most recent session.
                                let first = this
                                    .sessions
                                    .iter()
                                    .find(|s| s.quest_id.as_deref() == Some(qid.as_str()))
                                    .map(|s| s.id.clone());
                                if let Some(sid) = first {
                                    this.select_session(sid, window, cx);
                                }
                                cx.notify();
                            },
                        ))
                    })),
            )
    }

    fn render_sessions_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let lang = self.config.language;
        let visible: Vec<&Session> = self
            .sessions
            .iter()
            .filter(|s| {
                self.selected_quest_filter
                    .as_deref()
                    .is_none_or(|qf| s.quest_id.as_deref() == Some(qf))
            })
            .collect();

        div()
            .v_flex()
            .flex_1()
            .min_h_0()
            .gap_1()
            .child(
                div()
                    .h_flex()
                    .justify_between()
                    .items_center()
                    .child(section_label(t(lang, "セッション", "SESSIONS"), &theme))
                    .child(
                        Button::new("btn-new-session")
                            .xsmall()
                            .primary()
                            .tooltip("Ctrl+N")
                            .label(t(lang, "+ 新規", "+ New"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.create_new_session(window, cx);
                                cx.notify();
                            })),
                    ),
            )
            .when(visible.is_empty(), |el| {
                el.child(div().text_xs().text_color(theme.muted_foreground).child(t(
                    lang,
                    "セッションはまだありません",
                    "No sessions yet",
                )))
            })
            .child(
                div()
                    .id("sidebar-sessions-scroll")
                    .v_flex()
                    .flex_1()
                    .overflow_y_scroll()
                    .children(visible.into_iter().map(|s| {
                        let sid = s.id.clone();
                        let sid_rename = s.id.clone();
                        let sid_del = s.id.clone();
                        let selected = self.selected_session_id.as_deref() == Some(s.id.as_str());
                        let running = self.is_session_running(&s.id);
                        let waiting = self
                            .pending_permissions
                            .iter()
                            .any(|p| p.session_id == s.id);
                        let renaming = self.renaming_session_id.as_deref() == Some(s.id.as_str());
                        let dot = if waiting {
                            status_amber().into()
                        } else if running {
                            accent().into()
                        } else {
                            status_color(s.status)
                        };

                        if renaming {
                            return div()
                                .px_1()
                                .py(px(2.0))
                                .child(Input::new(&self.rename_input).xsmall())
                                .into_any_element();
                        }

                        list_row(
                            SharedString::from(format!("sess-{}", s.id)),
                            selected,
                            &theme,
                        )
                        .group("row")
                        .child(div().size(px(8.0)).flex_shrink_0().rounded_full().bg(dot))
                        .child(
                            div()
                                .v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(div().truncate().child(self.session_title(s)))
                                .child(
                                    div()
                                        .text_xs()
                                        .truncate()
                                        .text_color(theme.muted_foreground)
                                        .child(if waiting {
                                            t(lang, "承認待ち", "Needs approval").to_string()
                                        } else if running {
                                            t(lang, "応答中…", "Responding…").to_string()
                                        } else {
                                            format!(
                                                "{} · {}",
                                                self.provider_name(&s.provider_id),
                                                rel_time(s.updated_at, lang)
                                            )
                                        }),
                                ),
                        )
                        .child(self.delete_button(
                            format!("del-sess-{}", s.id),
                            format!("sess:{}", s.id),
                            &theme,
                            move |this, window, cx| {
                                this.delete_session(sid_del.clone(), window, cx)
                            },
                            cx,
                        ))
                        .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                            if ev.click_count() >= 2 {
                                this.start_rename(&sid_rename, window, cx);
                            } else {
                                this.select_session(sid.clone(), window, cx);
                            }
                            cx.notify();
                        }))
                        .into_any_element()
                    })),
            )
    }
}

impl SameAppView {
    /// Settings → Providers: connect, authenticate and test Antigravity,
    /// Claude Code, Codex, OpenCode and custom ACP agents.
    fn render_providers_settings(&self, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme().clone();
        let lang = self.config.language;

        div()
            .v_flex()
            .gap_4()
            .child(settings_header(
                SettingsSection::Providers.title(lang),
                t(
                    lang,
                    "Antigravity / Claude Code / Codex / OpenCode / カスタム ACP エージェントの接続・認証・テストを行います",
                    "Connect, authenticate, and test Antigravity, Claude Code, Codex, OpenCode, and custom ACP agents",
                ),
                &theme,
                Some(
                    Button::new("refresh-all-providers")
                        .small()
                        .primary()
                        .label(t(lang, "すべて再確認", "Refresh All"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.refresh_provider_checks();
                            cx.notify();
                        }))
                        .into_any_element(),
                ),
            ))
            // Provider cards
            .children(self.config.providers.iter().enumerate().map(|(idx, p)| {
                self.render_provider_card(idx, p, cx)
            }))
            // Add Custom ACP Agent card
            .child(
                div()
                    .v_flex()
                    .p_4()
                    .gap_3()
                    .rounded(px(8.0))
                    .bg(theme.sidebar)
                    .border_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .text_base()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(accent_text())
                            .child(t(
                                lang,
                                "+ カスタム ACP エージェントを登録",
                                "+ Register Custom ACP Agent",
                            )),
                    )
                    .child(
                        div()
                            .h_flex()
                            .gap_2()
                            .child(
                                div()
                                    .v_flex()
                                    .flex_1()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child("ID (e.g. my-agent)"),
                                    )
                                    .child(Input::new(&self.custom_id).small()),
                            )
                            .child(
                                div()
                                    .v_flex()
                                    .flex_1()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child(t(lang, "表示名", "Display Name")),
                                    )
                                    .child(Input::new(&self.custom_name).small()),
                            ),
                    )
                    .child(
                        div()
                            .h_flex()
                            .gap_2()
                            .child(
                                div()
                                    .v_flex()
                                    .flex_1()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child(t(lang, "実行コマンド", "Command")),
                                    )
                                    .child(Input::new(&self.custom_cmd).small()),
                            )
                            .child(
                                div()
                                    .v_flex()
                                    .flex_1()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child(t(
                                                lang,
                                                "引数 (スペース区切り)",
                                                "Args (space-separated)",
                                            )),
                                    )
                                    .child(Input::new(&self.custom_args).small()),
                            ),
                    )
                    .child(
                        div()
                            .v_flex()
                            .gap_1()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(t(lang, "説明", "Description")),
                            )
                            .child(Input::new(&self.custom_desc).small()),
                    )
                    .child(
                        div().h_flex().child(
                            Button::new("add-custom-acp")
                                .small()
                                .primary()
                                .label(t(lang, "ACP エージェントを追加", "Add ACP Agent"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    let id = this.custom_id.read(cx).value().trim().to_string();
                                    let cmd = this.custom_cmd.read(cx).value().trim().to_string();
                                    if id.is_empty() || cmd.is_empty() {
                                        this.note(
                                            NoteKind::Warning,
                                            t(this.config.language, "ID と実行コマンドは必須です", "ID and command are required"),
                                        );
                                    } else if this.config.find_provider(&id).is_some() {
                                        this.note(
                                            NoteKind::Error,
                                            format!("{}: {id}", t(this.config.language, "同じ ID のプロバイダが既にあります", "A provider with this ID already exists")),
                                        );
                                    } else {
                                        let name_raw =
                                            this.custom_name.read(cx).value().trim().to_string();
                                        let args_raw =
                                            this.custom_args.read(cx).value().trim().to_string();
                                        let desc =
                                            this.custom_desc.read(cx).value().trim().to_string();
                                        let cfg = ProviderConfig {
                                            id: id.clone(),
                                            name: if name_raw.is_empty() {
                                                id
                                            } else {
                                                name_raw
                                            },
                                            kind: ProviderKind::Acp,
                                            enabled: true,
                                            command: cmd,
                                            args: args_raw
                                                .split_whitespace()
                                                .map(str::to_string)
                                                .collect(),
                                            env: BTreeMap::new(),
                                            default_model: None,
                                            models: vec![],
                                            description: desc,
                                        };
                                        this.config.providers.push(cfg);
                                        this.save_config();
                                        this.custom_id
                                            .update(cx, |i, cx| i.set_value("", window, cx));
                                        this.custom_name
                                            .update(cx, |i, cx| i.set_value("", window, cx));
                                        this.custom_cmd
                                            .update(cx, |i, cx| i.set_value("", window, cx));
                                        this.custom_args
                                            .update(cx, |i, cx| i.set_value("", window, cx));
                                        this.custom_desc
                                            .update(cx, |i, cx| i.set_value("", window, cx));
                                        this.refresh_provider_checks();
                                        cx.notify();
                                    }
                                })),
                        ),
                    ),
            )
    }

    fn render_provider_card(
        &self,
        idx: usize,
        p: &ProviderConfig,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let lang = self.config.language;
        let check = self.provider_checks.get(&p.id);
        let connect_res = self.connect_results.get(&p.id);
        let login_res = self.login_results.get(&p.id);
        let caps = AgentManager::capabilities_for(p.kind);
        let is_default = self.config.default_provider_id == p.id;
        let is_editing = self.editing_provider_id.as_deref() == Some(p.id.as_str());
        let is_builtin = matches!(
            p.id.as_str(),
            "opencode" | "claude" | "codex" | "antigravity"
        );

        let (status_color, status_text) = if let Some((ok, msg)) = connect_res {
            (if *ok { status_green() } else { status_red() }, msg.clone())
        } else if let Some(chk) = check {
            (
                if chk.available {
                    status_green()
                } else {
                    status_amber()
                },
                if cfg!(windows) {
                    format!("{} · {}", chk.detail, chk.target)
                } else {
                    chk.detail.clone()
                },
            )
        } else {
            (
                gpui::rgb(0x888888),
                t(lang, "確認中...", "Checking...").to_string(),
            )
        };

        let mut cap_labels = Vec::new();
        if caps.supports_acp {
            cap_labels.push("ACP");
        }
        if caps.supports_streaming {
            cap_labels.push("Streaming");
        }
        if caps.supports_thinking {
            cap_labels.push("Thinking");
        }
        if caps.supports_tool_calls {
            cap_labels.push("Tools");
        }
        if caps.supports_permissions {
            cap_labels.push("Permissions");
        }
        if caps.supports_session_resume {
            cap_labels.push("Resume");
        }

        let p_test = p.clone();
        let p_login = p.clone();
        let p_edit = p.clone();
        let pid_default = p.id.clone();
        let pid_toggle = p.id.clone();
        let pid_remove = p.id.clone();
        let setup_running = self.setup_running.contains(&p.id);
        let login_preview = format!("$ {}", self.setup_command_preview(p, ProviderSetup::Login));
        let install_preview = format!(
            "$ {}",
            self.setup_command_preview(p, ProviderSetup::InstallCli)
        );
        // Offer the installer once the check says the CLI is missing.
        let show_install =
            AgentManager::install_script(p).is_some() && check.is_some_and(|c| !c.available);
        let armed_delete =
            self.confirm_delete.as_deref() == Some(format!("prov:{}", p.id).as_str());

        div()
            .v_flex()
            .p_4()
            .gap_2()
            .rounded(px(8.0))
            .bg(theme.sidebar)
            .border_1()
            .border_color(if is_default {
                gpui::Hsla::from(accent())
            } else {
                theme.border
            })
            // Top row: Name + Protocol badge + Status
            .child(
                div()
                    .h_flex()
                    .justify_between()
                    .items_center()
                    .child(
                        div()
                            .h_flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_base()
                                    .font_weight(FontWeight::BOLD)
                                    .child(p.name.clone()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .px_2()
                                    .py(px(2.0))
                                    .rounded(px(4.0))
                                    .bg(theme.background)
                                    .text_color(accent_text())
                                    .child(p.kind.label()),
                            )
                            .when(is_default, |el| {
                                el.child(
                                    div()
                                        .text_xs()
                                        .px_2()
                                        .py(px(2.0))
                                        .rounded(px(4.0))
                                        .bg(accent())
                                        .text_color(accent_foreground())
                                        .child(t(lang, "デフォルト", "Default")),
                                )
                            }),
                    )
                    .child(
                        div()
                            .h_flex()
                            .items_center()
                            .gap_1()
                            .child(div().text_xs().text_color(status_color).child("●"))
                            .child(div().text_xs().text_color(status_color).child(status_text)),
                    ),
            )
            // Description & command
            .when(!p.description.is_empty(), |el| {
                el.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(p.description.clone()),
                )
            })
            .child(
                div()
                    .text_xs()
                    .px_2()
                    .py_1()
                    .rounded(px(4.0))
                    .bg(theme.background)
                    .text_color(theme.muted_foreground)
                    .child(format!("$ {} {}", p.command, p.args.join(" "))),
            )
            // Capabilities row
            .child(
                div()
                    .h_flex()
                    .gap_1()
                    .flex_wrap()
                    .children(cap_labels.into_iter().map(|cap| {
                        div()
                            .text_xs()
                            .px_2()
                            .py(px(1.0))
                            .rounded(px(4.0))
                            .bg(theme.background)
                            .border_1()
                            .border_color(theme.border)
                            .text_color(theme.muted_foreground)
                            .child(cap)
                    })),
            )
            // Login output if any
            .when_some(login_res.cloned(), |el, msg| {
                el.child(
                    div()
                        .text_xs()
                        .p_2()
                        .rounded(px(4.0))
                        .bg(theme.background)
                        .border_1()
                        .border_color(gpui::Hsla::from(accent()))
                        .child(msg),
                )
            })
            // Inline edit form when editing
            .when(is_editing, |el| {
                let pid_save = p.id.clone();
                el.child(
                    div()
                        .v_flex()
                        .gap_2()
                        .p_3()
                        .rounded(px(6.0))
                        .bg(theme.background)
                        .border_1()
                        .border_color(theme.border)
                        .child(
                            div()
                                .h_flex()
                                .gap_2()
                                .child(
                                    div()
                                        .v_flex()
                                        .flex_1()
                                        .gap_1()
                                        .child(div().text_xs().child(t(
                                            lang,
                                            "コマンド",
                                            "Command",
                                        )))
                                        .child(Input::new(&self.edit_provider_cmd).small()),
                                )
                                .child(
                                    div()
                                        .v_flex()
                                        .flex_1()
                                        .gap_1()
                                        .child(div().text_xs().child(t(lang, "引数", "Args")))
                                        .child(Input::new(&self.edit_provider_args).small()),
                                )
                                .child(
                                    div()
                                        .v_flex()
                                        .flex_1()
                                        .gap_1()
                                        .child(div().text_xs().child(t(
                                            lang,
                                            "既定モデル",
                                            "Default Model",
                                        )))
                                        .child(Input::new(&self.edit_provider_model).small()),
                                ),
                        )
                        .child(
                            div()
                                .h_flex()
                                .gap_2()
                                .child(
                                    Button::new(SharedString::from(format!("save-edit-{}", p.id)))
                                        .xsmall()
                                        .primary()
                                        .label(t(lang, "保存", "Save"))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            let cmd = this
                                                .edit_provider_cmd
                                                .read(cx)
                                                .value()
                                                .trim()
                                                .to_string();
                                            let args = this
                                                .edit_provider_args
                                                .read(cx)
                                                .value()
                                                .split_whitespace()
                                                .map(str::to_string)
                                                .collect();
                                            let model = this
                                                .edit_provider_model
                                                .read(cx)
                                                .value()
                                                .trim()
                                                .to_string();
                                            if let Some(target) = this
                                                .config
                                                .providers
                                                .iter_mut()
                                                .find(|x| x.id == pid_save)
                                            {
                                                if !cmd.is_empty() {
                                                    target.command = cmd;
                                                }
                                                target.args = args;
                                                target.default_model = if model.is_empty() {
                                                    None
                                                } else {
                                                    Some(model)
                                                };
                                            }
                                            this.editing_provider_id = None;
                                            this.save_config();
                                            this.refresh_provider_checks();
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    Button::new(SharedString::from(format!(
                                        "cancel-edit-{}",
                                        p.id
                                    )))
                                    .xsmall()
                                    .ghost()
                                    .label(t(lang, "キャンセル", "Cancel"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.editing_provider_id = None;
                                        cx.notify();
                                    })),
                                ),
                        ),
                )
            })
            // Action buttons row
            .child(
                div()
                    .h_flex()
                    .gap_2()
                    .flex_wrap()
                    .child(
                        Button::new(SharedString::from(format!("connect-{}", p.id)))
                            .small()
                            .primary()
                            .label(t(lang, "接続テスト", "Test Connection"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.connect_results
                                    .insert(p_test.id.clone(), (true, "Connecting...".to_string()));
                                this.test_provider_connection(p_test.clone());
                                cx.notify();
                            })),
                    )
                    .when(show_install, |el| {
                        let p_install = p.clone();
                        el.child(
                            Button::new(SharedString::from(format!("install-{}", p.id)))
                                .small()
                                .primary()
                                .disabled(setup_running)
                                .tooltip(install_preview.clone())
                                .label(match lang {
                                    Language::Ja => {
                                        format!("CLI をインストール ({})", self.exec_label())
                                    }
                                    Language::En => format!("Install CLI ({})", self.exec_label()),
                                })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.run_provider_setup(
                                        p_install.clone(),
                                        ProviderSetup::InstallCli,
                                    );
                                    cx.notify();
                                })),
                        )
                    })
                    .child(
                        Button::new(SharedString::from(format!("login-{}", p.id)))
                            .small()
                            .ghost()
                            .disabled(setup_running)
                            .tooltip(login_preview)
                            .label(if setup_running {
                                t(lang, "ターミナルで操作中…", "Waiting for terminal…")
                            } else {
                                t(lang, "ログイン (ターミナル)", "Sign in (terminal)")
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.run_provider_setup(p_login.clone(), ProviderSetup::Login);
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new(SharedString::from(format!("edit-{}", p.id)))
                            .small()
                            .ghost()
                            .label(t(lang, "設定編集", "Configure"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.editing_provider_id = Some(p_edit.id.clone());
                                this.edit_provider_cmd.update(cx, |i, cx| {
                                    i.set_value(p_edit.command.clone(), window, cx);
                                });
                                this.edit_provider_args.update(cx, |i, cx| {
                                    i.set_value(p_edit.args.join(" "), window, cx);
                                });
                                this.edit_provider_model.update(cx, |i, cx| {
                                    i.set_value(
                                        p_edit.default_model.clone().unwrap_or_default(),
                                        window,
                                        cx,
                                    );
                                });
                                cx.notify();
                            })),
                    )
                    .when(!is_default, |el| {
                        el.child(
                            Button::new(SharedString::from(format!("default-{}", p.id)))
                                .small()
                                .ghost()
                                .label(t(lang, "デフォルトに設定", "Set Default"))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.config.default_provider_id = pid_default.clone();
                                    this.save_config();
                                    cx.notify();
                                })),
                        )
                    })
                    .child(
                        Button::new(SharedString::from(format!("enable-{}", p.id)))
                            .small()
                            .ghost()
                            .label(if p.enabled {
                                t(lang, "無効化", "Disable")
                            } else {
                                t(lang, "有効化", "Enable")
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(target) = this
                                    .config
                                    .providers
                                    .iter_mut()
                                    .find(|x| x.id == pid_toggle)
                                {
                                    target.enabled = !target.enabled;
                                }
                                this.save_config();
                                cx.notify();
                            })),
                    )
                    .when(!is_builtin, |el| {
                        el.child(
                            Button::new(SharedString::from(format!("remove-{}", p.id)))
                                .small()
                                .danger()
                                .label(if armed_delete {
                                    t(lang, "本当に削除?", "Really delete?")
                                } else {
                                    t(lang, "削除", "Delete")
                                })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if !this.confirm_or_arm_delete(format!("prov:{pid_remove}"), cx)
                                    {
                                        cx.notify();
                                        return;
                                    }
                                    if idx < this.config.providers.len()
                                        && this.config.providers[idx].id == pid_remove
                                    {
                                        this.config.providers.remove(idx);
                                        if this.config.default_provider_id == pid_remove {
                                            if let Some(first) = this.config.providers.first() {
                                                this.config.default_provider_id = first.id.clone();
                                            }
                                        }
                                        this.save_config();
                                    }
                                    cx.notify();
                                })),
                        )
                    }),
            )
            .into_any_element()
    }
}

/// Section heading for the settings screen: title, description and an
/// optional action on the right.
fn settings_header(
    title: &str,
    description: &str,
    theme: &Theme,
    action: Option<AnyElement>,
) -> Div {
    div()
        .h_flex()
        .justify_between()
        .items_center()
        .gap_4()
        .child(
            div()
                .v_flex()
                .gap_1()
                .min_w_0()
                .child(
                    div()
                        .text_xl()
                        .font_weight(FontWeight::BOLD)
                        .child(title.to_string()),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(description.to_string()),
                ),
        )
        .when_some(action, |el, a| el.child(div().flex_shrink_0().child(a)))
}

/// A bordered group of settings rows.
fn settings_card(theme: &Theme) -> Div {
    div()
        .v_flex()
        .rounded(px(8.0))
        .bg(theme.sidebar)
        .border_1()
        .border_color(theme.border)
}

/// One labelled setting: text on the left, the control on the right.
fn settings_row(
    label: &str,
    hint: &str,
    control: impl IntoElement,
    first: bool,
    theme: &Theme,
) -> Div {
    div()
        .h_flex()
        .justify_between()
        .items_center()
        .gap_4()
        .px_4()
        .py_3()
        .when(!first, |el| el.border_t_1().border_color(theme.border))
        .child(
            div()
                .v_flex()
                .gap_0p5()
                .min_w_0()
                .flex_1()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .child(label.to_string()),
                )
                .when(!hint.is_empty(), |el| {
                    el.child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(hint.to_string()),
                    )
                }),
        )
        .child(div().flex_shrink_0().child(control))
}

impl SameAppView {
    /// The settings screen: a section list on the left, the section's page
    /// on the right.
    fn render_settings_view(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let lang = self.config.language;
        let current = self.settings_section;

        let nav = div()
            .v_flex()
            .w(px(200.0))
            .h_full()
            .flex_shrink_0()
            .p_3()
            .gap_1()
            .border_r_1()
            .border_color(theme.border)
            .child(
                div()
                    .px_2()
                    .pb_2()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.muted_foreground)
                    .child(t(lang, "設定", "SETTINGS")),
            )
            .children(SettingsSection::ALL.into_iter().map(|section| {
                let selected = section == current;
                Button::new(SharedString::from(format!(
                    "settings-nav-{}",
                    section.title(Language::En)
                )))
                .ghost()
                .small()
                .selected(selected)
                .toggled(selected)
                .w_full()
                .justify_start()
                .icon(Icon::new(section.icon()).small())
                .label(section.title(lang))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.open_settings(section);
                    cx.notify();
                }))
            }));

        let body = match current {
            SettingsSection::General => self.render_general_settings(cx),
            SettingsSection::Appearance => self.render_appearance_settings(cx),
            SettingsSection::Providers => self.render_providers_settings(cx),
        };

        div()
            .h_flex()
            .flex_1()
            .h_full()
            .min_w_0()
            .bg(theme.background)
            .child(nav)
            .child(
                div()
                    .id(SharedString::from(format!(
                        "settings-page-scroll-{}",
                        current.title(Language::En)
                    )))
                    .flex_1()
                    .h_full()
                    .min_w_0()
                    .overflow_y_scroll()
                    .child(div().p_6().max_w(px(880.0)).child(body)),
            )
    }

    fn render_general_settings(&self, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme().clone();
        let lang = self.config.language;

        let language_control = div()
            .h_flex()
            .gap_1()
            .children([(Language::Ja, "日本語"), (Language::En, "English")].map(
                |(value, label)| {
                    Button::new(SharedString::from(format!("set-lang-{label}")))
                        .small()
                        .label(label)
                        .toggled(self.config.language == value)
                        .when(self.config.language == value, |b| b.primary())
                        .when(self.config.language != value, |b| b.ghost())
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.set_language(value, window, cx);
                            cx.notify();
                        }))
                },
            ));

        let view = cx.entity().downgrade();
        let default_id = self.config.default_provider_id.clone();
        let providers: Vec<(String, String, bool)> = self
            .config
            .providers
            .iter()
            .map(|p| (p.id.clone(), p.name.clone(), p.enabled))
            .collect();
        let default_provider_control = Button::new("settings-default-provider")
            .small()
            .ghost()
            .max_w(px(240.0))
            .overflow_hidden()
            .label(format!("{} ▾", self.provider_name(&default_id)))
            .dropdown_menu(move |menu, _, _| {
                let mut menu = menu;
                for (pid, name, enabled) in providers.iter() {
                    let v = view.clone();
                    let pid2 = pid.clone();
                    let label = if *enabled {
                        name.clone()
                    } else {
                        format!("{name}  ({})", t(lang, "無効", "disabled"))
                    };
                    menu = menu.item(
                        PopupMenuItem::new(label)
                            .checked(*pid == default_id)
                            .disabled(!*enabled)
                            .on_click(move |_, _, cx| {
                                let pid3 = pid2.clone();
                                let _ = v.update(cx, |this, cx| {
                                    if this
                                        .config
                                        .providers
                                        .iter()
                                        .any(|p| p.id == pid3 && p.enabled)
                                    {
                                        this.config.default_provider_id = pid3;
                                        this.save_config();
                                        cx.notify();
                                    }
                                });
                            }),
                    );
                }
                menu
            });

        let auto_approve = self.config.auto_approve_tools;
        let auto_approve_control = div()
            .h_flex()
            .gap_1()
            .children([(false, t(lang, "確認する", "Ask")), (true, t(lang, "自動承認", "Auto-approve"))].map(
                |(value, label)| {
                    Button::new(SharedString::from(format!("set-auto-approve-{value}")))
                        .small()
                        .label(label)
                        .toggled(auto_approve == value)
                        .when(auto_approve == value, |b| {
                            if value {
                                b.warning()
                            } else {
                                b.primary()
                            }
                        })
                        .when(auto_approve != value, |b| b.ghost())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.set_auto_approve(value);
                            cx.notify();
                        }))
                },
            ));

        let submit_key = self.config.submit_key;
        let mod_key = if cfg!(target_os = "macos") { "Cmd" } else { "Ctrl" };
        let submit_key_control = div().h_flex().gap_1().children(
            [
                (SubmitKey::Enter, "Enter".to_string()),
                (SubmitKey::ModEnter, format!("{mod_key}+Enter")),
            ]
            .map(|(value, label)| {
                Button::new(SharedString::from(format!("set-submit-key-{value:?}")))
                    .small()
                    .label(label)
                    .toggled(submit_key == value)
                    .when(submit_key == value, |b| b.primary())
                    .when(submit_key != value, |b| b.ghost())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.set_submit_key(value, window, cx);
                        cx.notify();
                    }))
            }),
        );
        let submit_key_hint = match (submit_key, lang) {
            (SubmitKey::Enter, Language::Ja) => "Enter で送信、Shift+Enter で改行".to_string(),
            (SubmitKey::Enter, Language::En) => {
                "Enter sends, Shift+Enter inserts a newline".to_string()
            }
            (SubmitKey::ModEnter, Language::Ja) => {
                format!("{mod_key}+Enter で送信、Enter で改行")
            }
            (SubmitKey::ModEnter, Language::En) => {
                format!("{mod_key}+Enter sends, Enter inserts a newline")
            }
        };

        div()
            .v_flex()
            .gap_4()
            .child(settings_header(
                SettingsSection::General.title(lang),
                t(
                    lang,
                    "言語や新しいセッションの既定値を設定します",
                    "Language and defaults for new sessions",
                ),
                &theme,
                None,
            ))
            .child(
                settings_card(&theme)
                    .child(settings_row(
                        t(lang, "言語", "Language"),
                        t(lang, "UI の表示言語", "Display language of the UI"),
                        language_control,
                        true,
                        &theme,
                    ))
                    .child(settings_row(
                        t(lang, "既定のプロバイダ", "Default provider"),
                        t(
                            lang,
                            "現在のセッションがないときに新しいセッションで使うエージェント",
                            "Agent used for new sessions when no session is selected",
                        ),
                        default_provider_control,
                        false,
                        &theme,
                    ))
                    .child(settings_row(
                        t(lang, "送信キー", "Send key"),
                        &submit_key_hint,
                        submit_key_control,
                        false,
                        &theme,
                    ))
                    .child(settings_row(
                        t(lang, "ツール実行の承認", "Tool permissions"),
                        t(
                            lang,
                            "全 ACP セッションの新しい単発承認リクエストを自動許可します。CLI エージェントでは次回実行時のプロバイダ権限モードを切り替えます",
                            "Automatically grants new single-use ACP permission requests across all sessions; for CLI agents, selects the provider's own permission mode on the next run",
                        ),
                        auto_approve_control,
                        false,
                        &theme,
                    )),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!(
                        "{}: {}",
                        t(lang, "設定ファイル", "Config file"),
                        self.paths.config_file.display()
                    )),
            )
    }

    fn render_appearance_settings(&self, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme().clone();
        let lang = self.config.language;

        let mode_control = div()
            .h_flex()
            .gap_1()
            .children(
                [
                    (ThemeMode::Dark, t(lang, "ダーク", "Dark")),
                    (ThemeMode::Light, t(lang, "ライト", "Light")),
                ]
                .map(|(value, label)| {
                    Button::new(SharedString::from(format!("set-theme-{label}")))
                        .small()
                        .label(label)
                        .toggled(self.config.theme == value)
                        .when(self.config.theme == value, |b| b.primary())
                        .when(self.config.theme != value, |b| b.ghost())
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.set_theme_mode(value, window, cx);
                            cx.notify();
                        }))
                }),
            );

        let swatches = div()
            .h_flex()
            .flex_wrap()
            .gap_2()
            .children(AccentColor::ALL.into_iter().map(|value| {
                let selected = self.config.accent == value;
                let (ja, en) = value.label();
                let color: Hsla = gpui::rgb(value.hex()).into();
                Button::new(SharedString::from(format!("set-accent-{en}")))
                    .ghost()
                    .selected(selected)
                    .toggled(selected)
                    .h_auto()
                    .w(px(76.0))
                    .py_2()
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(if selected { color } else { theme.border })
                    .child(
                        div()
                            .v_flex()
                            .items_center()
                            .gap_1p5()
                            .child(
                                div()
                                    .size(px(28.0))
                                    .rounded_full()
                                    .bg(color)
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .text_color(accent_foreground())
                                    .when(selected, |el| el.child(Icon::new(IconName::Check).small())),
                            )
                            .child(div().text_xs().child(t(lang, ja, en))),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.set_accent(value, window, cx);
                        cx.notify();
                    }))
            }));

        let preview = div()
            .h_flex()
            .items_center()
            .gap_2()
            .child(Button::new("accent-preview-primary").small().primary().label(t(
                lang,
                "プライマリ",
                "Primary",
            )))
            .child(
                div()
                    .text_xs()
                    .px_2()
                    .py(px(2.0))
                    .rounded(px(4.0))
                    .bg(accent())
                    .text_color(accent_foreground())
                    .child(t(lang, "デフォルト", "Default")),
            )
            .child(div().text_sm().text_color(accent_text()).child(t(
                lang,
                "アクセントテキスト",
                "Accent text",
            )));

        div()
            .v_flex()
            .gap_4()
            .child(settings_header(
                SettingsSection::Appearance.title(lang),
                t(
                    lang,
                    "テーマ・アクセントカラー・フォントを変更します",
                    "Theme, accent colour and fonts",
                ),
                &theme,
                None,
            ))
            .child(
                settings_card(&theme)
                    .child(settings_row(
                        t(lang, "テーマ", "Theme"),
                        t(lang, "ダーク / ライト", "Dark or light"),
                        mode_control,
                        true,
                        &theme,
                    ))
                    .child(
                        div()
                            .v_flex()
                            .gap_3()
                            .px_4()
                            .py_3()
                            .border_t_1()
                            .border_color(theme.border)
                            .child(
                                div()
                                    .v_flex()
                                    .gap_0p5()
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(FontWeight::MEDIUM)
                                            .child(t(lang, "テーマカラー", "Accent colour")),
                                    )
                                    .child(div().text_xs().text_color(theme.muted_foreground).child(
                                        t(
                                            lang,
                                            "ボタン・選択状態・強調表示に使う色",
                                            "Used for buttons, selection and highlights",
                                        ),
                                    )),
                            )
                            .child(swatches)
                            .child(preview),
                    ),
            )
            .child(
                settings_card(&theme)
                    .child(settings_row(
                        t(lang, "フォントサイズ", "Font size"),
                        t(
                            lang,
                            "UI 全体の文字の大きさ (コードも同じ比率で拡大縮小)",
                            "Base text size for the whole UI (code scales with it)",
                        ),
                        self.render_font_size_control(cx),
                        true,
                        &theme,
                    ))
                    .child(self.render_font_picker(false, cx))
                    .child(self.render_font_picker(true, cx)),
            )
    }

    fn render_font_size_control(&self, cx: &mut Context<Self>) -> Div {
        let lang = self.config.language;
        let overridden = self.config.font_size().is_some();
        let size = self
            .config
            .font_size()
            .unwrap_or_else(default_font_size)
            .round();

        div()
            .h_flex()
            .items_center()
            .gap_1()
            .child(
                Button::new("font-size-dec")
                    .small()
                    .ghost()
                    .label("−")
                    .disabled(size <= MIN_FONT_SIZE)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.set_font_size(Some(size - 1.0), window, cx);
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .w(px(56.0))
                    .text_center()
                    .text_sm()
                    .child(format!("{size:.0} px")),
            )
            .child(
                Button::new("font-size-inc")
                    .small()
                    .ghost()
                    .label("+")
                    .disabled(size >= MAX_FONT_SIZE)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.set_font_size(Some(size + 1.0), window, cx);
                        cx.notify();
                    })),
            )
            .child(
                Button::new("font-size-reset")
                    .small()
                    .ghost()
                    .label(t(lang, "既定に戻す", "Reset"))
                    .disabled(!overridden)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.set_font_size(None, window, cx);
                        cx.notify();
                    })),
            )
    }

    /// Picker for the UI font (`mono == false`) or the code font: a
    /// dropdown of installed fonts, each drawn in itself, and a preview.
    fn render_font_picker(&self, mono: bool, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme().clone();
        let lang = self.config.language;
        let (current, default_name) = if mono {
            (&self.config.mono_font, default_mono_font())
        } else {
            (&self.config.ui_font, default_ui_font())
        };
        let overridden = current.is_some();
        let family: SharedString = current
            .clone()
            .unwrap_or_else(|| default_name.to_string())
            .into();
        let open = self.font_menu_open == Some(mono);

        let (label, hint, sample) = if mono {
            (
                t(lang, "コードフォント", "Code font"),
                t(
                    lang,
                    "コードブロック・ツール出力・ファイルエディタ・ログ",
                    "Code blocks, tool output, the file editor and logs",
                ),
                "fn main() { let x = [0, O, 1, l, I]; } // コメント",
            )
        } else {
            (
                t(lang, "UI フォント", "UI font"),
                t(lang, "メニュー・チャット・ボタンなど", "Menus, chat, buttons and so on"),
                "あいうえお 漢字 カタカナ — The quick brown fox 0123",
            )
        };

        div()
            .v_flex()
            .gap_2()
            .px_4()
            .py_3()
            .border_t_1()
            .border_color(theme.border)
            .child(
                div()
                    .h_flex()
                    .justify_between()
                    .items_center()
                    .gap_4()
                    .child(
                        div()
                            .v_flex()
                            .gap_0p5()
                            .min_w_0()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(label),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(hint),
                            ),
                    )
                    .child(
                        div()
                            .h_flex()
                            .items_center()
                            .gap_2()
                            .flex_shrink_0()
                            // Dropdown trigger: the current font, in itself.
                            .child(
                                div()
                                    .id(SharedString::from(format!("font-trigger-{mono}")))
                                    .h_flex()
                                    .items_center()
                                    .justify_between()
                                    .gap_2()
                                    .w(px(280.0))
                                    .px_3()
                                    .py_1p5()
                                    .rounded(px(6.0))
                                    .border_1()
                                    .border_color(if open { theme.ring } else { theme.border })
                                    .bg(theme.background)
                                    .cursor_pointer()
                                    .hover(|s| s.bg(theme.list_hover))
                                    .child(
                                        div()
                                            .min_w_0()
                                            .truncate()
                                            .text_sm()
                                            .font_family(family.clone())
                                            .child(if overridden {
                                                family.to_string()
                                            } else {
                                                format!("{family} ({})", t(lang, "既定", "default"))
                                            }),
                                    )
                                    .child(
                                        div()
                                            .flex_shrink_0()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child(if open { "▴" } else { "▾" }),
                                    )
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.toggle_font_menu(mono, window, cx);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new(SharedString::from(format!("font-reset-{mono}")))
                                    .small()
                                    .ghost()
                                    .label(t(lang, "既定に戻す", "Reset"))
                                    .disabled(!overridden)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.set_font(mono, None, window, cx);
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            // Dropdown: a search box over every installed font, virtualized
            // so only the rows on screen are laid out (and their fonts
            // loaded).
            .when(open, |el| {
                let empty = self.font_rows.is_empty();
                el.child(
                    div()
                        .id(SharedString::from(format!("font-menu-{mono}")))
                        .v_flex()
                        .gap_1()
                        .p_1()
                        .rounded(px(6.0))
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.background)
                        .shadow_md()
                        // The settings page scrolls too: keep wheel events
                        // that land on the dropdown away from it, so the
                        // list (not the page) scrolls.
                        .occlude()
                        .on_scroll(|_, _, cx| cx.stop_propagation())
                        .child(Input::new(&self.font_search).small())
                        .child(
                            div()
                                .h(px(300.0))
                                .when(empty, |el| {
                                    el.p_2()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(t(
                                            lang,
                                            "一致するフォントがありません",
                                            "No matching fonts",
                                        ))
                                })
                                .when(!empty, |el| {
                                    el.child(
                                        list(
                                            self.font_list.clone(),
                                            cx.processor(|this, ix: usize, _window, cx| {
                                                this.render_font_option(ix, cx)
                                            }),
                                        )
                                        .size_full(),
                                    )
                                }),
                        ),
                )
            })
            .child(
                div()
                    .px_2()
                    .py_1p5()
                    .rounded(px(4.0))
                    .bg(theme.background)
                    .text_sm()
                    .font_family(family)
                    .child(sample),
            )
    }

    /// Row `ix` of the open font dropdown (see `font_rows`): the platform
    /// default or an installed font, drawn in its own font.
    fn render_font_option(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(mono) = self.font_menu_open else {
            return div().into_any_element();
        };
        let theme = cx.theme().clone();
        let lang = self.config.language;
        let current = if mono {
            self.config.mono_font.as_deref()
        } else {
            self.config.ui_font.as_deref()
        };
        let (value, family, label): (Option<String>, SharedString, String) =
            match self.font_rows.get(ix) {
                Some(None) => {
                    let name = if mono {
                        default_mono_font()
                    } else {
                        default_ui_font()
                    };
                    (
                        None,
                        name.into(),
                        format!("{} — {name}", t(lang, "既定", "Default")),
                    )
                }
                Some(Some(i)) => match self.font_names.get(*i) {
                    Some(name) => (Some(name.clone()), name.clone().into(), name.clone()),
                    None => return div().into_any_element(),
                },
                None => return div().into_any_element(),
            };
        let selected = current == value.as_deref();
        let sample = if mono { "0O1lI {}[]" } else { "あア漢 Aa" };

        div()
            .py(px(1.0))
            .child(
                list_row(SharedString::from(format!("font-opt-{ix}")), selected, &theme)
                    .justify_between()
                    .font_family(family)
                    .child(div().min_w_0().truncate().child(label))
                    .child(
                        div()
                            .h_flex()
                            .items_center()
                            .gap_2()
                            .flex_shrink_0()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(sample),
                            )
                            .when(selected, |el| el.child(Icon::new(IconName::Check).small())),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.set_font(mono, value.clone(), window, cx);
                        this.font_menu_open = None;
                        cx.notify();
                    })),
            )
            .into_any_element()
    }
}

impl SameAppView {
    fn render_chat_header(
        &self,
        session: &Session,
        running: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let lang = self.config.language;
        let sid_title = session.id.clone();

        div()
            .v_flex()
            .px_4()
            .py_2()
            .gap_2()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .h_flex()
                    .justify_between()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .id("session-title")
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_base()
                            .font_weight(FontWeight::BOLD)
                            .cursor_pointer()
                            .child(self.session_title(session))
                            .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                                if ev.click_count() >= 2 {
                                    this.start_rename(&sid_title, window, cx);
                                    cx.notify();
                                }
                            })),
                    )
                    .child(
                        div()
                            .h_flex()
                            .items_center()
                            .gap_1()
                            .flex_shrink_0()
                            // Where this project's agents run (Windows only: WSL or not).
                            .when(cfg!(windows), |el| {
                                let mode = self.exec_mode();
                                let view = cx.entity().downgrade();
                                el.child(
                                    Button::new("exec-target-menu")
                                        .xsmall()
                                        .ghost()
                                        .disabled(running)
                                        .tooltip(t(
                                            lang,
                                            "エージェントを実行する環境",
                                            "Where agents run for this project",
                                        ))
                                        .label(format!("⚙ {} ▾", self.exec_label()))
                                        .dropdown_menu(move |menu, _, _| {
                                            let mut menu = menu;
                                            for (m, label) in [
                                                (
                                                    ExecMode::Auto,
                                                    t(
                                                        lang,
                                                        "自動 (フォルダの場所で判断)",
                                                        "Auto (by folder location)",
                                                    ),
                                                ),
                                                (ExecMode::Host, "Windows"),
                                                (ExecMode::Wsl, "WSL"),
                                            ] {
                                                let v = view.clone();
                                                menu = menu.item(
                                                    PopupMenuItem::new(label)
                                                        .checked(mode == m)
                                                        .on_click(move |_, _, cx| {
                                                            let _ = v.update(cx, |this, cx| {
                                                                this.set_exec_mode(m);
                                                                cx.notify();
                                                            });
                                                        }),
                                                );
                                            }
                                            menu
                                        }),
                                )
                            }),
                    ),
            )
    }

    /// Switches a session to another agent. The other agent cannot resume
    /// this one's conversation or use its model names, so both are cleared.
    fn switch_session_provider(&mut self, sid: &str, pid: &str) {
        let changed = self
            .sessions
            .iter()
            .find(|x| x.id == sid)
            .is_some_and(|x| x.provider_id != pid);
        if !changed {
            return;
        }
        let _ =
            self.storage
                .update_session_meta(sid, None, Some(pid), Some(None), Some(None), None);
        if let Some(s) = self.sessions.iter_mut().find(|x| x.id == sid) {
            s.provider_id = pid.to_string();
            s.external_session_id = None;
            s.model = None;
            s.effort = None;
        }
        let _ = self.storage.set_session_effort(sid, None);
        self.agent_current_model.remove(sid);
        self.slash_commands.remove(sid);
    }

    /// The row under the prompt: agent and model pickers (like the Codex /
    /// Claude Code composers), the auto-approve toggle, and key hints.
    fn render_composer_bar(
        &self,
        session: &Session,
        running: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let lang = self.config.language;
        let view = cx.entity().downgrade();
        let provider = self.config.find_provider(&session.provider_id).cloned();
        let choices: Vec<ModelInfo> = provider
            .as_ref()
            .map(|p| self.model_choices(p))
            .unwrap_or_default();
        let default_model = provider.as_ref().and_then(|p| p.default_model.clone());
        let current_model = session.model.clone();
        // What "Default" resolves to, by display name when known.
        let agent_model = self
            .agent_default_model
            .get(&session.provider_id)
            .or_else(|| self.agent_current_model.get(&session.id))
            .map(|id| {
                choices
                    .iter()
                    .find(|m| &m.id == id)
                    .map(|m| m.name.clone())
                    .unwrap_or_else(|| id.clone())
            });
        let discover_provider = provider.clone();
        let discovering = self.models_discovering.contains(&session.provider_id);
        let sid = session.id.clone();

        // Agent picker
        let providers: Vec<(String, String, Option<bool>)> = self
            .config
            .providers
            .iter()
            .filter(|p| p.enabled)
            .map(|p| {
                (
                    p.id.clone(),
                    p.name.clone(),
                    self.provider_checks.get(&p.id).map(|c| c.available),
                )
            })
            .collect();
        let active_pid = session.provider_id.clone();
        let avail = self
            .provider_checks
            .get(&session.provider_id)
            .map(|c| c.available);
        let dot: Hsla = match avail {
            Some(true) => status_green().into(),
            Some(false) => status_red().into(),
            None => gpui::rgb(0x888888).into(),
        };
        let provider_view = view.clone();
        let provider_sid = sid.clone();
        let provider_button = Button::new("composer-provider-menu")
            .xsmall()
            .ghost()
            .disabled(running)
            .tooltip(t(lang, "エージェントを切り替え", "Switch agent"))
            .child(div().size(px(6.0)).rounded_full().bg(dot))
            .label(format!("{} ▾", self.provider_name(&session.provider_id)))
            .dropdown_menu(move |menu, _, _| {
                let mut menu = menu;
                for (pid, name, ok) in providers.iter() {
                    let v = provider_view.clone();
                    let pid2 = pid.clone();
                    let sid2 = provider_sid.clone();
                    let label = if *ok == Some(false) {
                        format!("{name}  ⚠")
                    } else {
                        name.clone()
                    };
                    menu = menu.item(
                        PopupMenuItem::new(label)
                            .checked(*pid == active_pid)
                            .on_click(move |_, _, cx| {
                                let pid3 = pid2.clone();
                                let sid3 = sid2.clone();
                                let _ = v.update(cx, |this, cx| {
                                    this.switch_session_provider(&sid3, &pid3);
                                    cx.notify();
                                });
                            }),
                    );
                }
                menu = menu.item(PopupMenuItem::separator());
                let v = provider_view.clone();
                menu.item(
                    PopupMenuItem::new(t(lang, "プロバイダを管理…", "Manage providers…")).on_click(
                        move |_, _, cx| {
                            let _ = v.update(cx, |this, cx| {
                                this.open_settings(SettingsSection::Providers);
                                cx.notify();
                            });
                        },
                    ),
                )
            });

        // Effort picker (only when the agent reports effort levels)
        let (efforts, default_effort) = self.effort_choices(session);
        let current_effort = session.effort.clone();
        let effort_button = (!efforts.is_empty() || current_effort.is_some()).then(|| {
            let effort_view = view.clone();
            let label = match (&current_effort, &default_effort) {
                (Some(e), _) => e.clone(),
                (None, Some(d)) => format!("{} ({d})", t(lang, "既定", "default")),
                (None, None) => t(lang, "既定", "default").to_string(),
            };
            Button::new("composer-effort-menu")
                .xsmall()
                .ghost()
                .tooltip(t(
                    lang,
                    "思考の深さ (effort) を選択",
                    "Pick the reasoning effort",
                ))
                .label(format!("{}: {label} ▾", t(lang, "思考", "Effort")))
                .dropdown_menu(move |menu, _, _| {
                    let mut menu = menu;
                    let v = effort_view.clone();
                    let default_label = match &default_effort {
                        Some(d) => format!("{} ({d})", t(lang, "既定", "Default")),
                        None => t(lang, "既定 (エージェント任せ)", "Default (agent decides)")
                            .to_string(),
                    };
                    menu = menu.item(
                        PopupMenuItem::new(default_label)
                            .checked(current_effort.is_none())
                            .on_click(move |_, _, cx| {
                                let _ = v.update(cx, |this, cx| {
                                    this.set_session_effort(None);
                                    cx.notify();
                                });
                            }),
                    );
                    if !efforts.is_empty() {
                        menu = menu.item(PopupMenuItem::separator());
                    }
                    for e in efforts.iter() {
                        let v = effort_view.clone();
                        let id = e.clone();
                        menu = menu.item(
                            PopupMenuItem::new(e.clone())
                                .checked(current_effort.as_deref() == Some(e.as_str()))
                                .on_click(move |_, _, cx| {
                                    let id2 = id.clone();
                                    let _ = v.update(cx, |this, cx| {
                                        this.set_session_effort(Some(id2));
                                        cx.notify();
                                    });
                                }),
                        );
                    }
                    menu
                })
        });

        // Model picker
        let model_view = view.clone();
        let model_button = Button::new("composer-model-menu")
            .xsmall()
            .ghost()
            .tooltip(t(
                lang,
                "モデルを選択 (/model <名前> でも変更できます)",
                "Pick a model (or type /model <name>)",
            ))
            .label(format!("{} ▾", self.model_label(session)))
            .dropdown_menu(move |menu, _, _| {
                let mut menu = menu.scrollable(true).max_h(px(420.0));
                let default_label = match default_model.as_ref().or(agent_model.as_ref()) {
                    Some(m) => format!("{} ({m})", t(lang, "既定", "Default")),
                    None => {
                        t(lang, "既定 (エージェント任せ)", "Default (agent decides)").to_string()
                    }
                };
                let v = model_view.clone();
                menu = menu.item(
                    PopupMenuItem::new(default_label)
                        .checked(current_model.is_none())
                        .on_click(move |_, _, cx| {
                            let _ = v.update(cx, |this, cx| {
                                this.set_session_model(None);
                                cx.notify();
                            });
                        }),
                );
                // Long lists (OpenCode reports 100+) also get a shortcut to
                // the filterable `/model ` suggestions.
                if choices.len() > 20 {
                    let v = model_view.clone();
                    menu = menu.item(
                        PopupMenuItem::new(match lang {
                            Language::Ja => format!("検索… ({} 件)", choices.len()),
                            Language::En => format!("Search… ({})", choices.len()),
                        })
                        .on_click(move |_, window, cx| {
                            let _ = v.update(cx, |this, cx| {
                                this.prompt_input.update(cx, |ta, cx| {
                                    ta.set_value("/model ", window, cx);
                                    ta.focus(window, cx);
                                });
                                this.prompt_empty = false;
                                this.prompt_is_command = true;
                                cx.notify();
                            });
                        }),
                    );
                }
                {
                    let v = model_view.clone();
                    let provider = discover_provider.clone();
                    menu = menu.item(
                        PopupMenuItem::new(if discovering {
                            t(lang, "モデル一覧を取得中…", "Fetching models…")
                        } else {
                            t(lang, "モデル一覧を更新", "Refresh model list")
                        })
                        .disabled(discovering || provider.is_none())
                        .on_click(move |_, _, cx| {
                            if let Some(p) = provider.clone() {
                                let _ = v.update(cx, |this, cx| {
                                    this.start_model_discovery(p, true);
                                    cx.notify();
                                });
                            }
                        }),
                    );
                }
                if !choices.is_empty() {
                    menu = menu.item(PopupMenuItem::separator());
                }
                for m in choices.iter() {
                    let v = model_view.clone();
                    let id = m.id.clone();
                    let label = if m.name == m.id {
                        m.name.clone()
                    } else {
                        format!("{}  ({})", m.name, m.id)
                    };
                    menu = menu.item(
                        PopupMenuItem::new(label)
                            .checked(current_model.as_deref() == Some(m.id.as_str()))
                            .on_click(move |_, _, cx| {
                                let id2 = id.clone();
                                let _ = v.update(cx, |this, cx| {
                                    this.set_session_model(Some(id2));
                                    cx.notify();
                                });
                            }),
                    );
                }
                menu
            });

        div()
            .h_flex()
            .items_center()
            .justify_between()
            .gap_2()
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_1()
                    .min_w_0()
                    .child(provider_button)
                    .child(model_button)
                    .children(effort_button)
                    .child(
                        Button::new("toggle-auto-approve")
                            .xsmall()
                            .toggled(self.config.auto_approve_tools)
                            .when(self.config.auto_approve_tools, |b| b.warning())
                            .when(!self.config.auto_approve_tools, |b| b.ghost())
                            .tooltip(t(
                                lang,
                                "全 ACP セッションの新しい単発承認リクエストを自動許可します（CLI エージェントは次回実行から反映）",
                                "Applies globally: automatically grants new single-use ACP permission requests across all sessions, and sets the permission mode for future CLI runs",
                            ))
                            .label(if self.config.auto_approve_tools {
                                t(lang, "自動承認: ON", "Auto-approve: ON")
                            } else {
                                t(lang, "自動承認: OFF", "Auto-approve: OFF")
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.set_auto_approve(!this.config.auto_approve_tools);
                                cx.notify();
                            })),
                    )
                    .when(running, |el| el.child(t(lang, "応答中…", "Responding…"))),
            )
    }

    /// Suggestions shown above the prompt while a `/command` is typed:
    /// `/model` (handled here) and the agent's own commands, or the model
    /// list once `/model ` is typed.
    fn render_command_suggestions(
        &self,
        session: &Session,
        slash_cmds: &[SlashCommandInfo],
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.prompt_is_command {
            return None;
        }
        let theme = cx.theme().clone();
        let lang = self.config.language;
        let text = self.prompt_input.read(cx).value().to_string();

        // (fill-in text, label, description, is current)
        let mut rows: Vec<(String, String, String, bool)> = Vec::new();
        if let Some(arg) = text.strip_prefix("/model ") {
            let q = arg.trim().to_lowercase();
            let current = session.model.clone();
            let choices = self
                .config
                .find_provider(&session.provider_id)
                .map(|p| self.model_choices(p))
                .unwrap_or_default();
            if q.is_empty() || "default".starts_with(&q) {
                rows.push((
                    "/model default".to_string(),
                    t(lang, "既定", "default").to_string(),
                    t(
                        lang,
                        "エージェントの既定モデル",
                        "The agent's default model",
                    )
                    .to_string(),
                    current.is_none(),
                ));
            }
            for m in choices.iter().filter(|m| {
                q.is_empty()
                    || m.id.to_lowercase().contains(&q)
                    || m.name.to_lowercase().contains(&q)
            }) {
                let desc = if m.name != m.id {
                    if m.description.is_empty() {
                        m.id.clone()
                    } else {
                        format!("{} · {}", m.id, m.description)
                    }
                } else {
                    m.description.clone()
                };
                rows.push((
                    format!("/model {}", m.id),
                    m.name.clone(),
                    desc,
                    current.as_deref() == Some(m.id.as_str()),
                ));
            }
            if rows.is_empty() {
                rows.push((
                    text.trim_end().to_string(),
                    arg.trim().to_string(),
                    t(
                        lang,
                        "一覧にないモデル名をそのまま使います (Enter)",
                        "Use this model name as typed (Enter)",
                    )
                    .to_string(),
                    false,
                ));
            }
        } else {
            let q = text.trim_start_matches('/').trim().to_lowercase();
            if q.contains(char::is_whitespace) {
                return None;
            }
            if "model".starts_with(&q) {
                rows.push((
                    "/model ".to_string(),
                    "/model".to_string(),
                    t(lang, "モデルを切り替える", "Switch the model").to_string(),
                    false,
                ));
            }
            for c in slash_cmds
                .iter()
                .filter(|c| c.name != "model" && c.name.to_lowercase().starts_with(&q))
            {
                rows.push((
                    format!("/{} ", c.name),
                    format!("/{}", c.name),
                    c.description.clone(),
                    false,
                ));
            }
        }
        if rows.is_empty() {
            return None;
        }

        Some(
            div()
                .id("command-suggestions")
                .v_flex()
                .w_full()
                .max_h(px(240.0))
                .overflow_y_scroll()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .bg(theme.sidebar)
                .children(rows.into_iter().take(50).enumerate().map(
                    |(i, (fill, label, desc, current))| {
                        list_row(SharedString::from(format!("cmd-sug-{i}")), current, &theme)
                            .gap_2()
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(label),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(desc),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                // A full "/model <id>" applies at once; a
                                // command name is filled in for its arguments.
                                if fill.starts_with("/model ") && fill.trim() != "/model" {
                                    this.try_model_command(&fill, window, cx);
                                } else {
                                    this.prompt_input.update(cx, |ta, cx| {
                                        ta.set_value(fill.clone(), window, cx);
                                        ta.focus(window, cx);
                                    });
                                    this.prompt_empty = false;
                                    this.prompt_is_command = true;
                                }
                                cx.notify();
                            }))
                    },
                ))
                .into_any_element(),
        )
    }

    fn render_chat_view(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let lang = self.config.language;

        let Some(session) = self.current_session().cloned() else {
            return div()
                .flex_1()
                .h_full()
                .v_flex()
                .justify_center()
                .items_center()
                .gap_3()
                .bg(theme.background)
                .child(gpui::img(crate::brand::mascot()).size(px(112.0)))
                .child(
                    div()
                        .text_2xl()
                        .font_weight(FontWeight::BOLD)
                        .text_color(accent_text())
                        .child("SAME"),
                )
                .child(div().text_sm().text_color(theme.muted_foreground).child(t(
                    lang,
                    "左のプロジェクト欄の + から作業フォルダを追加してください",
                    "Add a working folder with + under Projects to get started",
                )))
                .child(
                    Button::new("empty-add-project")
                        .primary()
                        .label(t(lang, "プロジェクトを追加", "Add project"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.show_new_project_form = true;
                            let cwd = std::env::current_dir()
                                .map(|p| p.to_string_lossy().to_string())
                                .unwrap_or_default();
                            this.new_project_path.update(cx, |i, cx| {
                                i.set_value(cwd, window, cx);
                                i.focus(window, cx);
                            });
                            cx.notify();
                        })),
                )
                .into_any_element();
        };

        let running = self.is_session_running(&session.id);

        let quest_banner = session.quest_id.as_deref().and_then(|qid| {
            self.quests
                .iter()
                .find(|q| q.id == qid)
                .map(|q| (q.title.clone(), q.goal.clone()))
        });

        let slash_cmds = self
            .slash_commands
            .get(&session.id)
            .cloned()
            .unwrap_or_default();

        let provider_missing = self
            .provider_checks
            .get(&session.provider_id)
            .is_some_and(|c| !c.available);

        let suggestions = self.render_command_suggestions(&session, &slash_cmds, cx);
        let composer_bar = self.render_composer_bar(&session, running, cx);

        div()
            .v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(theme.background)
            .child(self.render_chat_header(&session, running, cx))
            .when(provider_missing, |el| {
                let detail = self
                    .provider_checks
                    .get(&session.provider_id)
                    .map(|c| c.detail.clone())
                    .unwrap_or_default();
                el.child(
                    div()
                        .h_flex()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .px_4()
                        .py_1()
                        .bg(theme.warning.opacity(0.12))
                        .child(div().text_xs().text_color(status_amber()).truncate().child(
                            format!(
                                "⚠ {} — {detail}",
                                match lang {
                                    Language::Ja => format!(
                                        "{} を起動できません",
                                        self.provider_name(&session.provider_id)
                                    ),
                                    Language::En => format!(
                                        "{} is not available",
                                        self.provider_name(&session.provider_id)
                                    ),
                                }
                            ),
                        ))
                        .child(
                            Button::new("fix-provider")
                                .xsmall()
                                .ghost()
                                .label(t(lang, "設定を開く", "Open settings"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.open_settings(SettingsSection::Providers);
                                    cx.notify();
                                })),
                        ),
                )
            })
            // Quest banner
            .when_some(quest_banner, |el, (title, goal)| {
                el.child(
                    div()
                        .h_flex()
                        .gap_2()
                        .px_4()
                        .py_1()
                        .bg(theme.sidebar)
                        .border_b_1()
                        .border_color(theme.border)
                        .child(
                            div()
                                .text_xs()
                                .font_weight(FontWeight::BOLD)
                                .text_color(accent_text())
                                .child(format!("🎯 {title}")),
                        )
                        .child(
                            div()
                                .text_xs()
                                .truncate()
                                .text_color(theme.muted_foreground)
                                .child(goal),
                        ),
                )
            })
            // Message feed (virtualized)
            .child({
                self.sync_feed();
                list(
                    self.feed.clone(),
                    cx.processor(|this, ix: usize, _window, cx| this.render_feed_item(ix, cx)),
                )
                .flex_1()
                .min_h_0()
                .w_full()
            })
            // Prompt composer
            .child(
                div().v_flex().px_4().pt_2().pb_3().gap_1().child(
                    div()
                        .v_flex()
                        .w_full()
                        .max_w(px(920.0))
                        .mx_auto()
                        .gap_1()
                        .when_some(suggestions, |el, s| el.child(s))
                        .child(
                            div()
                                .h_flex()
                                .gap_2()
                                .items_end()
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .child(Textarea::new(&self.prompt_input)),
                                )
                                .child(if running {
                                    Button::new("btn-cancel-prompt")
                                        .danger()
                                        .tooltip("Esc")
                                        .label(t(lang, "■ 停止", "■ Stop"))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.cancel_current();
                                            cx.notify();
                                        }))
                                } else {
                                    Button::new("btn-send-prompt")
                                        .primary()
                                        .disabled(self.prompt_empty)
                                        .tooltip("Enter")
                                        .label(t(lang, "送信", "Send"))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.send_current_prompt(window, cx);
                                            cx.notify();
                                        }))
                                }),
                        )
                        .child(composer_bar),
                ),
            )
            .into_any_element()
    }

    /// Wraps one feed item in the centered reading column.
    fn feed_column(&self, ix: usize, child: impl IntoElement) -> AnyElement {
        div()
            .w_full()
            .child(
                div()
                    .v_flex()
                    .w_full()
                    .max_w(px(920.0))
                    .mx_auto()
                    .px_5()
                    .when(ix == 0, |d| d.pt_4())
                    .pb_4()
                    .child(child),
            )
            .into_any_element()
    }

    /// Renders feed item `ix`: a stored message, or (last) the trailing
    /// item with the live reply, permission prompts and notices.
    fn render_feed_item(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        if let Some(m) = self.messages.get(ix) {
            let el = self.render_message(m, None, cx);
            return self.feed_column(ix, el);
        }
        let Some(session) = self.current_session().cloned() else {
            return div().into_any_element();
        };
        let theme = cx.theme().clone();
        let lang = self.config.language;
        let running = self.is_session_running(&session.id);
        let live = self.live_drafts.get(&session.id);
        let pending_perms: Vec<PermissionRequest> = self
            .pending_permissions
            .iter()
            .filter(|r| r.session_id == session.id)
            .cloned()
            .collect();

        let tail = div()
            .v_flex()
            .w_full()
            .gap_4()
            .when(self.messages.is_empty() && live.is_none(), |el| {
                let proj_label = self
                    .current_project()
                    .map(|p| p.workdir.clone())
                    .unwrap_or_default();
                el.child(
                    div()
                        .v_flex()
                        .items_center()
                        .justify_center()
                        .py_12()
                        .gap_2()
                        .child(gpui::img(crate::brand::mascot()).size(px(96.0)))
                        .child(
                            div()
                                .text_2xl()
                                .font_weight(FontWeight::BOLD)
                                .text_color(accent_text())
                                .child("SAME"),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(proj_label),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(match lang {
                                    Language::Ja => format!(
                                        "{} に話しかけてみましょう。ファイル欄の @ でファイルを参照できます",
                                        self.provider_name(&session.provider_id)
                                    ),
                                    Language::En => format!(
                                        "Ask {} anything. Use @ in the file panel to reference files",
                                        self.provider_name(&session.provider_id)
                                    ),
                                }),
                        ),
                )
            })
            .when_some(live, |el, draft| {
                let m = Message {
                    id: format!("live-{}", session.id),
                    session_id: session.id.clone(),
                    role: MessageRole::Assistant,
                    content: draft.content.clone(),
                    thinking: if draft.thinking.is_empty() {
                        None
                    } else {
                        Some(draft.thinking.clone())
                    },
                    tool_calls: draft.tool_calls.clone(),
                    provider_id: Some(session.provider_id.clone()),
                    model: session.model.clone(),
                    created_at: Utc::now(),
                };
                el.child(self.render_message(&m, Some(!pending_perms.is_empty()), cx))
            })
            .children(
                pending_perms
                    .into_iter()
                    .map(|req| self.render_permission(req, cx)),
            )
            .when(
                !running
                    && session.status == SessionStatus::Interrupted
                    && self
                        .messages
                        .last()
                        .is_some_and(|m| m.role == MessageRole::User),
                |el| {
                    el.child(div().text_xs().text_color(theme.muted_foreground).child(t(
                        lang,
                        "前回の応答はアプリ終了で中断されました。もう一度送信してください",
                        "The last reply was interrupted when the app closed. Send again to retry",
                    )))
                },
            );
        self.feed_column(ix, tail)
    }

    fn render_permission(&self, req: PermissionRequest, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let lang = self.config.language;
        let has_reject = req.options.iter().any(|o| o.kind.starts_with("reject"));
        // Without a description, show what the tool is about to do.
        let detail = if req.description.trim().is_empty() {
            req.tool_call_id.as_ref().and_then(|tid| {
                self.live_drafts
                    .get(&req.session_id)
                    .and_then(|d| d.tool_calls.iter().find(|tc| &tc.id == tid))
                    .and_then(|tc| tc.input.clone())
            })
        } else {
            Some(req.description.clone())
        };
        let req_id_cancel = req.request_id.clone();
        div()
            .v_flex()
            .p_3()
            .gap_2()
            .rounded(px(8.0))
            .bg(theme.sidebar)
            .border_1()
            .border_color(gpui::Hsla::from(status_amber()))
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::BOLD)
                    .text_color(status_amber())
                    .child(format!(
                        "⚠ {}",
                        t(lang, "ツール実行の許可が必要です", "Permission needed")
                    )),
            )
            .child(div().text_sm().child(req.title.clone()))
            .when_some(detail, |el, detail| {
                el.child(
                    div()
                        .id(SharedString::from(format!("perm-desc-{}", req.request_id)))
                        .max_h(px(220.0))
                        .overflow_y_scroll()
                        .text_xs()
                        .p_2()
                        .rounded(px(4.0))
                        .bg(theme.background)
                        .font_family(theme.mono_font_family.clone())
                        .child(detail),
                )
            })
            .child(
                div()
                    .h_flex()
                    .gap_2()
                    .flex_wrap()
                    .children(req.options.into_iter().map(|opt| {
                        let req_id = req.request_id.clone();
                        let opt_id = opt.id.clone();
                        let kind = opt.kind.clone();
                        Button::new(SharedString::from(format!("perm-{}-{}", req_id, opt_id)))
                            .small()
                            .map(|b| match kind.as_str() {
                                "allow_once" => b.primary(),
                                k if k.starts_with("reject") => b.danger(),
                                _ => b.outline(),
                            })
                            .label(opt.name)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.pending_permissions.retain(|p| p.request_id != req_id);
                                let mgr = this.agent_manager.clone();
                                let rid = req_id.clone();
                                let oid = opt_id.clone();
                                this.rt.spawn(async move {
                                    mgr.respond_permission(&rid, Some(oid)).await;
                                });
                                cx.notify();
                            }))
                    }))
                    .when(!has_reject, |el| {
                        el.child(
                            Button::new(SharedString::from(format!(
                                "perm-cancel-{}",
                                req_id_cancel
                            )))
                            .small()
                            .danger()
                            .label(t(lang, "拒否", "Deny"))
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.pending_permissions
                                        .retain(|p| p.request_id != req_id_cancel);
                                    let mgr = this.agent_manager.clone();
                                    let rid = req_id_cancel.clone();
                                    this.rt.spawn(async move {
                                        mgr.respond_permission(&rid, None).await;
                                    });
                                    cx.notify();
                                },
                            )),
                        )
                    }),
            )
            .into_any_element()
    }

    /// Renders one message. `live` is `Some(waiting_for_permission)` for the
    /// in-progress assistant reply.
    fn render_message(
        &self,
        msg: &Message,
        live: Option<bool>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let lang = self.config.language;
        let is_user = msg.role == MessageRole::User;
        let copy_text = split_error(&msg.content).0.to_string();

        if is_user {
            return div()
                .h_flex()
                .justify_end()
                .group("msg")
                .child(
                    div()
                        .v_flex()
                        .max_w(gpui::relative(0.85))
                        .gap_1()
                        .items_end()
                        .child(
                            div()
                                .px_3()
                                .py_2()
                                .rounded(px(10.0))
                                .bg(theme.secondary)
                                .text_sm()
                                .child(
                                    TextView::markdown(
                                        SharedString::from(format!("md-{}", msg.id)),
                                        user_markdown(&msg.content),
                                    )
                                    .selectable(true),
                                ),
                        )
                        .child(
                            div()
                                .h_flex()
                                .gap_2()
                                .items_center()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .opacity(0.0)
                                .group_hover("msg", |s| s.opacity(1.0))
                                .child(
                                    msg.created_at
                                        .with_timezone(&Local)
                                        .format("%H:%M")
                                        .to_string(),
                                )
                                .child(
                                    Button::new(SharedString::from(format!("copy-{}", msg.id)))
                                        .xsmall()
                                        .ghost()
                                        .label(t(lang, "コピー", "Copy"))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                copy_text.clone(),
                                            ));
                                            this.note(
                                                NoteKind::Info,
                                                t(this.config.language, "コピーしました", "Copied"),
                                            );
                                            cx.notify();
                                        })),
                                ),
                        ),
                )
                .into_any_element();
        }

        let is_live = live.is_some();
        let waiting_perm = live == Some(true);
        let think_key = msg.id.clone();
        let show_thinking = self.expanded_thinking.contains(&think_key);
        let provider_label = msg
            .provider_id
            .as_deref()
            .map(|p| self.provider_name(p))
            .unwrap_or_else(|| "Assistant".to_string());
        let (body, error) = split_error(&msg.content);
        let body = body.to_string();
        let error = error.map(str::to_string);

        div()
            .v_flex()
            .gap_2()
            .group("msg")
            // Header
            .child(
                div()
                    .h_flex()
                    .justify_between()
                    .items_center()
                    .child(
                        div()
                            .h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(status_green())
                                    .child(provider_label),
                            )
                            .when_some(msg.model.clone(), |el, m| {
                                el.child(
                                    div().text_xs().text_color(theme.muted_foreground).child(m),
                                )
                            })
                            .when(!is_live, |el| {
                                el.child(
                                    div().text_xs().text_color(theme.muted_foreground).child(
                                        msg.created_at
                                            .with_timezone(&Local)
                                            .format("%H:%M")
                                            .to_string(),
                                    ),
                                )
                            }),
                    )
                    .when(!is_live && !body.trim().is_empty(), |el| {
                        el.child(
                            div()
                                .opacity(0.0)
                                .group_hover("msg", |s| s.opacity(1.0))
                                .child(
                                    Button::new(SharedString::from(format!("copy-{}", msg.id)))
                                        .xsmall()
                                        .ghost()
                                        .label(t(lang, "コピー", "Copy"))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                copy_text.clone(),
                                            ));
                                            this.note(
                                                NoteKind::Info,
                                                t(this.config.language, "コピーしました", "Copied"),
                                            );
                                            cx.notify();
                                        })),
                                ),
                        )
                    }),
            )
            // Collapsible Thinking
            .when_some(
                msg.thinking
                    .as_ref()
                    .filter(|s| !s.trim().is_empty())
                    .cloned(),
                |el, thinking| {
                    let key = think_key.clone();
                    let thinking_now = is_live && body.is_empty() && msg.tool_calls.is_empty();
                    el.child(
                        div()
                            .v_flex()
                            .gap_1()
                            .child(
                                div()
                                    .id(SharedString::from(format!("think-toggle-{key}")))
                                    .h_flex()
                                    .gap_1()
                                    .items_center()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .cursor_pointer()
                                    .hover(|s| s.text_color(theme.foreground))
                                    .child(if show_thinking { "▾" } else { "▸" })
                                    .child(if thinking_now {
                                        t(lang, "思考中…", "Thinking…")
                                    } else {
                                        t(lang, "思考過程", "Thinking")
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.feed.remeasure();
                                        if !this.expanded_thinking.remove(&key) {
                                            this.expanded_thinking.insert(key.clone());
                                        }
                                        cx.notify();
                                    })),
                            )
                            .when(show_thinking, |el| {
                                el.child(
                                    div()
                                        .text_xs()
                                        .pl_3()
                                        .border_l_2()
                                        .border_color(theme.border)
                                        .text_color(theme.muted_foreground)
                                        .child(thinking),
                                )
                            }),
                    )
                },
            )
            // Tool calls
            .when(!msg.tool_calls.is_empty(), |el| {
                el.child(
                    div().v_flex().gap_1().children(
                        msg.tool_calls
                            .iter()
                            .enumerate()
                            .map(|(i, tc)| self.render_tool_call(msg, i, tc, cx)),
                    ),
                )
            })
            // Message content
            .when(!body.trim().is_empty(), |el| {
                el.child(
                    div().text_sm().child(
                        TextView::markdown(
                            SharedString::from(format!("md-{}", msg.id)),
                            body.clone(),
                        )
                        .selectable(true),
                    ),
                )
            })
            .when_some(error, |el, err| {
                el.child(
                    div()
                        .p_2()
                        .rounded(px(6.0))
                        .border_1()
                        .border_color(gpui::Hsla::from(status_red()))
                        .bg(theme.danger.opacity(0.08))
                        .text_xs()
                        .text_color(status_red())
                        .font_family(theme.mono_font_family.clone())
                        .child(err.clone()),
                )
                .when(err.to_lowercase().contains("auth"), |el| {
                    el.child(
                        div().h_flex().child(
                            Button::new(SharedString::from(format!("auth-hint-{}", msg.id)))
                                .xsmall()
                                .outline()
                                .label(t(
                                    lang,
                                    "認証が必要です — プロバイダ接続を開く",
                                    "Authentication needed — open Providers",
                                ))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.open_settings(SettingsSection::Providers);
                                    cx.notify();
                                })),
                        ),
                    )
                })
            })
            .when(is_live, |el| {
                // A bigger hop while nothing has come back yet.
                let idle = body.is_empty() && msg.tool_calls.is_empty() && !waiting_perm;
                el.child(
                    div()
                        .h_flex()
                        .gap_2()
                        .items_center()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(crate::brand::mascot_loader(
                            SharedString::from(format!("loader-{}", msg.id)),
                            if idle { 36.0 } else { 18.0 },
                            theme.foreground,
                        ))
                        .child(if waiting_perm {
                            t(lang, "許可を待っています", "Waiting for your approval")
                        } else if body.is_empty() && msg.tool_calls.is_empty() {
                            t(lang, "考えています…", "Working…")
                        } else {
                            t(lang, "生成中…", "Generating…")
                        }),
                )
            })
            .into_any_element()
    }

    fn render_tool_call(
        &self,
        msg: &Message,
        i: usize,
        tc: &ToolCallRecord,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let lang = self.config.language;
        let tc_key = format!("{}-{}-{}", msg.id, tc.id, i);
        let expanded = self.expanded_tools.contains(&tc_key);
        let (badge, color): (&str, Hsla) = match tc.status {
            ToolCallStatus::Completed => ("✓", status_green().into()),
            ToolCallStatus::Failed => ("✗", status_red().into()),
            ToolCallStatus::Denied => ("⊘", status_red().into()),
            ToolCallStatus::Pending => ("○", status_amber().into()),
            ToolCallStatus::InProgress => ("◐", accent_text().into()),
        };
        let key_click = tc_key.clone();
        let title = if tc.title.trim().is_empty() {
            tc.kind.clone().unwrap_or_else(|| "tool".to_string())
        } else {
            tc.title.clone()
        };
        let mono = theme.mono_font_family.clone();
        div()
            .v_flex()
            .rounded(px(6.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.sidebar)
            .child(
                div()
                    .id(SharedString::from(format!("tc-{tc_key}")))
                    .h_flex()
                    .gap_2()
                    .items_center()
                    .px_2()
                    .py_1()
                    .text_xs()
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.list_hover))
                    .child(div().text_color(theme.muted_foreground).child(if expanded {
                        "▾"
                    } else {
                        "▸"
                    }))
                    .child(div().text_color(color).child(badge))
                    .when_some(tc.kind.clone(), |el, k| {
                        el.child(
                            div()
                                .px_1()
                                .rounded(px(3.0))
                                .bg(theme.background)
                                .text_color(theme.muted_foreground)
                                .child(k),
                        )
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_family(mono.clone())
                            .child(title),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.feed.remeasure();
                        if !this.expanded_tools.remove(&key_click) {
                            this.expanded_tools.insert(key_click.clone());
                        }
                        cx.notify();
                    })),
            )
            .when(expanded, |el| {
                let section = |label: &str, text: &str, id: String| {
                    let total = text.chars().count();
                    let mut preview: String =
                        text.chars().take(TOOL_OUTPUT_PREVIEW_CHARS).collect();
                    if total > TOOL_OUTPUT_PREVIEW_CHARS {
                        preview.push_str(&match lang {
                            Language::Ja => format!(
                                "\n… (残り {} 文字を省略)",
                                total - TOOL_OUTPUT_PREVIEW_CHARS
                            ),
                            Language::En => format!(
                                "\n… ({} more chars truncated)",
                                total - TOOL_OUTPUT_PREVIEW_CHARS
                            ),
                        });
                    }
                    div()
                        .v_flex()
                        .gap_1()
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(label.to_string()),
                        )
                        .child(
                            div()
                                .id(SharedString::from(id))
                                .max_h(px(260.0))
                                .overflow_y_scroll()
                                .p_2()
                                .rounded(px(4.0))
                                .bg(theme.background)
                                .text_xs()
                                .font_family(mono.clone())
                                .child(preview),
                        )
                };
                el.child(
                    div()
                        .v_flex()
                        .gap_2()
                        .p_2()
                        .border_t_1()
                        .border_color(theme.border)
                        .when_some(tc.input.clone(), |el, inp| {
                            el.child(section(
                                t(lang, "入力", "Input"),
                                &inp,
                                format!("tci-{tc_key}"),
                            ))
                        })
                        .when_some(tc.output.clone(), |el, out| {
                            el.child(section(
                                t(lang, "出力", "Output"),
                                &out,
                                format!("tco-{tc_key}"),
                            ))
                        })
                        .when(tc.input.is_none() && tc.output.is_none(), |el| {
                            el.child(div().text_xs().text_color(theme.muted_foreground).child(t(
                                lang,
                                "詳細はありません",
                                "No details",
                            )))
                        }),
                )
            })
            .into_any_element()
    }

    fn render_right_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let lang = self.config.language;

        let tab = |id: &'static str, label: String, active: bool| {
            Button::new(id)
                .xsmall()
                .when(active, |b| b.primary())
                .when(!active, |b| b.ghost())
                .label(label)
        };

        div()
            .v_flex()
            .w(px(380.0))
            .h_full()
            .flex_shrink_0()
            .bg(theme.sidebar)
            .border_l_1()
            .border_color(theme.border)
            .p_3()
            .gap_2()
            // Tab bar
            .child(
                div()
                    .h_flex()
                    .justify_between()
                    .items_center()
                    .child(
                        div()
                            .h_flex()
                            .gap_1()
                            .child(
                                tab(
                                    "tab-files",
                                    t(lang, "ファイル", "Files").into(),
                                    self.right_tab == RightTab::Files,
                                )
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.right_tab = RightTab::Files;
                                        cx.notify();
                                    },
                                )),
                            )
                            .when(self.git_info.is_repo, |el| {
                                let changes = self
                                    .git_info
                                    .status
                                    .lines()
                                    .filter(|l| !l.trim().is_empty())
                                    .count();
                                el.child(
                                    tab(
                                        "tab-git",
                                        if changes > 0 {
                                            format!("Git ({changes})")
                                        } else {
                                            "Git".to_string()
                                        },
                                        self.right_tab == RightTab::Git,
                                    )
                                    .on_click(cx.listener(
                                        |this, _, _, cx| {
                                            this.right_tab = RightTab::Git;
                                            this.reload_workspace_files(cx);
                                            cx.notify();
                                        },
                                    )),
                                )
                            })
                            .child(
                                tab(
                                    "tab-logs",
                                    t(lang, "ログ", "Logs").into(),
                                    self.right_tab == RightTab::Logs,
                                )
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.right_tab = RightTab::Logs;
                                        cx.notify();
                                    },
                                )),
                            ),
                    )
                    .child(
                        Button::new("refresh-right-panel")
                            .xsmall()
                            .ghost()
                            .tooltip(t(lang, "再読み込み", "Refresh"))
                            .label("↻")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.reload_workspace_files(cx);
                                cx.notify();
                            })),
                    ),
            )
            .child(match self.right_tab {
                RightTab::Files => self.render_files_tab(cx).into_any_element(),
                RightTab::Git => self.render_git_tab(cx).into_any_element(),
                RightTab::Logs => self.render_logs_tab(cx).into_any_element(),
            })
    }

    fn render_files_tab(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let lang = self.config.language;
        let filtering = !self.file_filter_input.read(cx).value().trim().is_empty();
        let file_open = self.opened_file_path.is_some();

        div()
            .v_flex()
            .flex_1()
            .min_h_0()
            .gap_2()
            .child(Input::new(&self.file_filter_input).xsmall().cleanable(true))
            // File tree
            .child(
                div()
                    .id("files-tree-scroll")
                    .v_flex()
                    .when(file_open, |d| d.h(px(220.0)).flex_shrink_0())
                    .when(!file_open, |d| d.flex_1().min_h_0())
                    .overflow_y_scroll()
                    .when(self.file_entries.is_empty(), |el| {
                        el.child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .p_2()
                                .child(if filtering {
                                    t(lang, "一致するファイルはありません", "No matching files")
                                } else {
                                    t(lang, "ファイルがありません", "No files")
                                }),
                        )
                    })
                    .children(self.file_entries.iter().enumerate().map(|(i, entry)| {
                        let name = if filtering {
                            entry.rel_path.clone()
                        } else {
                            entry
                                .path
                                .file_name()
                                .map(|n| n.to_string_lossy().to_string())
                                .unwrap_or_else(|| entry.rel_path.clone())
                        };
                        let path = entry.path.clone();
                        let is_dir = entry.is_dir;
                        let expanded = self.expanded_dirs.contains(&entry.path);
                        let selected = self.opened_file_path.as_ref() == Some(&entry.path);
                        let icon = if is_dir {
                            if expanded {
                                "▾"
                            } else {
                                "▸"
                            }
                        } else {
                            " "
                        };
                        list_row(
                            SharedString::from(format!("file-entry-{i}")),
                            selected,
                            &theme,
                        )
                        .py(px(2.0))
                        .text_xs()
                        .pl(px(8.0 + entry.depth as f32 * 14.0))
                        .child(
                            div()
                                .w(px(10.0))
                                .flex_shrink_0()
                                .text_color(theme.muted_foreground)
                                .child(icon),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .when(is_dir, |d| d.font_weight(FontWeight::SEMIBOLD))
                                .child(name),
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                if is_dir {
                                    if !this.expanded_dirs.remove(&path) {
                                        this.expanded_dirs.insert(path.clone());
                                    }
                                    this.reload_workspace_files(cx);
                                } else {
                                    this.open_file(path.clone(), window, cx);
                                }
                                cx.notify();
                            },
                        ))
                    })),
            )
            // File viewer / editor when a file is open
            .when_some(self.opened_file_path.clone(), |el, path| {
                let rel = self
                    .current_project()
                    .and_then(|p| path.strip_prefix(&p.workdir).ok())
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .to_string();
                let ref_path = path.clone();
                el.child(div().h(px(1.0)).bg(theme.border))
                    .child(
                        div()
                            .h_flex()
                            .justify_between()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_xs()
                                    .font_weight(FontWeight::BOLD)
                                    .child(if self.file_dirty {
                                        format!("● {rel}")
                                    } else {
                                        rel
                                    }),
                            )
                            .child(
                                div()
                                    .h_flex()
                                    .gap_1()
                                    .flex_shrink_0()
                                    .child(
                                        Button::new("insert-file-ref")
                                            .xsmall()
                                            .ghost()
                                            .tooltip(t(
                                                lang,
                                                "プロンプトにファイル参照を挿入",
                                                "Reference this file in the prompt",
                                            ))
                                            .label("@")
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.insert_file_reference(&ref_path, window, cx);
                                                cx.notify();
                                            })),
                                    )
                                    .child(
                                        Button::new("save-opened-file")
                                            .xsmall()
                                            .when(self.file_dirty, |b| b.primary())
                                            .when(!self.file_dirty, |b| b.ghost())
                                            .disabled(!self.file_dirty)
                                            .tooltip("Ctrl+S")
                                            .label(t(lang, "保存", "Save"))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.save_open_file(cx);
                                                cx.notify();
                                            })),
                                    )
                                    .child(
                                        Button::new("close-opened-file")
                                            .xsmall()
                                            .ghost()
                                            .tooltip(t(lang, "閉じる", "Close"))
                                            .label("✕")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                if this.file_dirty {
                                                    this.note(
                                                        NoteKind::Warning,
                                                        t(
                                                            this.config.language,
                                                            "未保存の変更を破棄しました",
                                                            "Discarded unsaved changes",
                                                        ),
                                                    );
                                                }
                                                this.opened_file_path = None;
                                                this.file_dirty = false;
                                                cx.notify();
                                            })),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .font_family(theme.mono_font_family.clone())
                            .text_xs()
                            .child(Textarea::new(&self.file_editor_input).h_full()),
                    )
            })
    }

    fn render_git_tab(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let lang = self.config.language;
        let mono = theme.mono_font_family.clone();

        div()
            .id("git-tab-scroll")
            .v_flex()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .gap_2()
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::BOLD)
                    .text_color(accent_text())
                    .child(format!("⎇ {}", self.git_info.branch)),
            )
            .child(section_label(t(lang, "変更ファイル", "Changes"), &theme))
            .child(
                div()
                    .v_flex()
                    .p_2()
                    .rounded(px(4.0))
                    .bg(theme.background)
                    .text_xs()
                    .font_family(mono.clone())
                    .when(self.git_info.status.trim().is_empty(), |el| {
                        el.child(t(lang, "変更はありません", "Working tree clean"))
                    })
                    .children(
                        self.git_info
                            .status
                            .lines()
                            .filter(|l| !l.trim().is_empty())
                            .map(|line| {
                                let code = line.get(..2).unwrap_or("").trim();
                                let color: Hsla = match code {
                                    "??" | "A" | "AM" => status_green().into(),
                                    "D" => status_red().into(),
                                    _ => status_amber().into(),
                                };
                                div().text_color(color).child(line.to_string())
                            }),
                    ),
            )
            .child(section_label("Diff", &theme))
            .when(self.git_info.diff.trim().is_empty(), |el| {
                el.child(div().text_xs().text_color(theme.muted_foreground).child(t(
                    lang,
                    "差分はありません (未追跡ファイルは含みません)",
                    "No diff (untracked files are not shown)",
                )))
            })
            .child(div().v_flex().text_xs().font_family(mono).children(
                self.git_info.diff.lines().take(1500).map(|line| {
                    let (color, bg) = if line.starts_with('+') && !line.starts_with("+++") {
                        (
                            Hsla::from(status_green()),
                            Some(Hsla::from(status_green()).opacity(0.08)),
                        )
                    } else if line.starts_with('-') && !line.starts_with("---") {
                        (
                            Hsla::from(status_red()),
                            Some(Hsla::from(status_red()).opacity(0.08)),
                        )
                    } else if line.starts_with("@@") {
                        (Hsla::from(accent_text()), None)
                    } else if line.starts_with("diff ") {
                        (theme.foreground, Some(theme.secondary))
                    } else {
                        (theme.muted_foreground, None)
                    };
                    div()
                        .px_1()
                        .text_color(color)
                        .when_some(bg, |d, bg| d.bg(bg))
                        .child(if line.is_empty() {
                            " ".to_string()
                        } else {
                            line.to_string()
                        })
                }),
            ))
    }

    fn render_logs_tab(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let lang = self.config.language;
        div()
            .v_flex()
            .flex_1()
            .min_h_0()
            .gap_2()
            .child(
                div()
                    .h_flex()
                    .justify_between()
                    .items_center()
                    .child(div().text_xs().text_color(theme.muted_foreground).child(t(
                        lang,
                        "エージェントの stderr 出力",
                        "Agent stderr output",
                    )))
                    .child(
                        Button::new("clear-logs")
                            .xsmall()
                            .ghost()
                            .label(t(lang, "クリア", "Clear"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.logs.clear();
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .id("logs-tab-scroll")
                    .v_flex()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_2()
                    .rounded(px(4.0))
                    .bg(theme.background)
                    .font_family(theme.mono_font_family.clone())
                    .text_xs()
                    .when(self.logs.is_empty(), |el| {
                        el.child(div().text_color(theme.muted_foreground).child(t(
                            lang,
                            "ログはまだありません",
                            "No logs yet",
                        )))
                    })
                    .children(self.logs.iter().map(|line| div().child(line.clone()))),
            )
    }
}

impl Focusable for SameAppView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// Height of the custom title bar. Slightly taller than gpui-component's
/// default so it reads like the Codex / Claude desktop chrome.
const TITLE_BAR_HEIGHT: gpui::Pixels = px(40.0);

/// A borderless, icon-only button for the title bar.
fn title_icon_button(
    id: &'static str,
    icon: IconName,
    active: bool,
    theme: &Theme,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .size(px(28.0))
        .rounded(px(6.0))
        .cursor_pointer()
        .text_color(if active {
            theme.foreground
        } else {
            theme.muted_foreground
        })
        .when(active, |d| d.bg(theme.list_active))
        .hover(|s| s.bg(theme.list_hover).text_color(theme.foreground))
        // Keep the press for the button: without this the title bar would
        // start a window drag on the slightest pointer motion.
        .on_mouse_down(gpui::MouseButton::Left, |_, window, cx| {
            window.prevent_default();
            cx.stop_propagation();
        })
        .child(Icon::new(icon).small())
}

impl SameAppView {
    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let lang = self.config.language;

        // Breadcrumb: the current project (the session title is already the
        // chat header), or the current screen.
        let mut crumbs: Vec<String> = Vec::new();
        match self.active_view {
            MainView::Chat => {
                if let Some(p) = self.current_project() {
                    crumbs.push(p.name.clone());
                }
            }
            MainView::Settings => {
                crumbs.push(t(lang, "設定", "Settings").into());
                crumbs.push(self.settings_section.title(lang).into());
            }
        }
        let crumb_count = crumbs.len();

        TitleBar::new()
            .h(TITLE_BAR_HEIGHT)
            .bg(theme.sidebar)
            .border_color(theme.border)
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_3()
                    .min_w_0()
                    .flex_1()
                    .child(
                        div()
                            .h_flex()
                            .items_center()
                            .gap_2()
                            .flex_shrink_0()
                            .child(gpui::img(crate::brand::mascot()).size(px(22.0)))
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("SAME"),
                            ),
                    )
                    .child(
                        div()
                            .h_flex()
                            .items_center()
                            .gap_1p5()
                            .min_w_0()
                            .text_sm()
                            .children(crumbs.into_iter().enumerate().flat_map(|(i, c)| {
                                let last = i + 1 == crumb_count;
                                let mut out: Vec<AnyElement> = Vec::new();
                                if i > 0 {
                                    out.push(
                                        div()
                                            .flex_shrink_0()
                                            .text_color(theme.muted_foreground)
                                            .opacity(0.6)
                                            .child("/")
                                            .into_any_element(),
                                    );
                                }
                                out.push(
                                    div()
                                        .min_w_0()
                                        .truncate()
                                        .text_color(if last {
                                            theme.foreground
                                        } else {
                                            theme.muted_foreground
                                        })
                                        .child(c)
                                        .into_any_element(),
                                );
                                out
                            })),
                    ),
            )
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_0p5()
                    .pr_2()
                    .flex_shrink_0()
                    .child(
                        title_icon_button("tb-lang", IconName::Globe, false, &theme)
                            .tooltip(move |window, cx| {
                                gpui_component::tooltip::Tooltip::new(t(
                                    lang,
                                    "Switch to English",
                                    "日本語に切り替え",
                                ))
                                .build(window, cx)
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                let next = match this.config.language {
                                    Language::Ja => Language::En,
                                    Language::En => Language::Ja,
                                };
                                this.set_language(next, window, cx);
                                cx.notify();
                            })),
                    )
                    .child(
                        title_icon_button(
                            "tb-theme",
                            match self.config.theme {
                                ThemeMode::Dark => IconName::Sun,
                                ThemeMode::Light => IconName::Moon,
                            },
                            false,
                            &theme,
                        )
                        .tooltip(move |window, cx| {
                            gpui_component::tooltip::Tooltip::new(t(
                                lang,
                                "テーマ切替",
                                "Toggle theme",
                            ))
                            .build(window, cx)
                        })
                        .on_click(cx.listener(|this, _, window, cx| {
                            let next = match this.config.theme {
                                ThemeMode::Dark => ThemeMode::Light,
                                ThemeMode::Light => ThemeMode::Dark,
                            };
                            this.set_theme_mode(next, window, cx);
                            cx.notify();
                        })),
                    )
                    .child(
                        title_icon_button(
                            "tb-settings",
                            IconName::Settings,
                            self.active_view == MainView::Settings,
                            &theme,
                        )
                        .tooltip(move |window, cx| {
                            gpui_component::tooltip::Tooltip::new(t(lang, "設定", "Settings"))
                                .build(window, cx)
                        })
                        .on_click(cx.listener(|this, _, _, cx| {
                            if this.active_view == MainView::Settings {
                                this.active_view = MainView::Chat;
                            } else {
                                let section = this.settings_section;
                                this.open_settings(section);
                            }
                            cx.notify();
                        })),
                    )
                    .when(self.active_view == MainView::Chat, |el| {
                        el.child(
                            title_icon_button(
                                "tb-right-panel",
                                IconName::PanelRight,
                                self.show_right_panel,
                                &theme,
                            )
                            .tooltip(move |window, cx| {
                                gpui_component::tooltip::Tooltip::new(t(
                                    lang,
                                    "ファイル / Git パネル (Ctrl+B)",
                                    "Files / Git panel (Ctrl+B)",
                                ))
                                .build(window, cx)
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_right_panel = !this.show_right_panel;
                                cx.notify();
                            })),
                        )
                    }),
            )
    }
}

impl Render for SameAppView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.reload_open_file_pending) {
            self.reload_open_file_from_disk(window, cx);
        }
        for (kind, msg) in std::mem::take(&mut self.pending_notes) {
            let note = match kind {
                NoteKind::Info => Notification::info(msg),
                NoteKind::Success => Notification::success(msg),
                NoteKind::Warning => Notification::warning(msg),
                NoteKind::Error => Notification::error(msg),
            };
            window.push_notification(note, cx);
        }

        let theme = cx.theme().clone();
        div()
            .id("same-root")
            .track_focus(&self.focus_handle)
            .key_context("SameApp")
            .on_action(cx.listener(|this, _: &NewSession, window, cx| {
                this.create_new_session(window, cx);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &SaveFile, _, cx| {
                if this.opened_file_path.is_some() && this.file_dirty {
                    this.save_open_file(cx);
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &FocusPrompt, window, cx| {
                this.active_view = MainView::Chat;
                this.prompt_input.update(cx, |s, cx| s.focus(window, cx));
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &SendPrompt, window, cx| {
                let focused = this
                    .prompt_input
                    .read(cx)
                    .focus_handle(cx)
                    .is_focused(window);
                if this.config.submit_key == SubmitKey::ModEnter && focused {
                    this.send_current_prompt(window, cx);
                    cx.notify();
                } else {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleRightPanel, _, cx| {
                this.show_right_panel = !this.show_right_panel;
                cx.notify();
            }))
            .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, window, cx| {
                if ev.keystroke.key == "escape" {
                    let prompt_focused = this
                        .prompt_input
                        .read(cx)
                        .focus_handle(cx)
                        .is_focused(window);
                    if this.renaming_session_id.take().is_some()
                        || this.font_menu_open.take().is_some()
                    {
                        cx.notify();
                    } else if prompt_focused
                        && this
                            .selected_session_id
                            .as_deref()
                            .is_some_and(|s| this.is_session_running(s))
                    {
                        this.cancel_current();
                        cx.notify();
                    }
                }
            }))
            .v_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(self.render_title_bar(cx))
            .child(
                div()
                    .h_flex()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(self.render_sidebar(cx))
                    .child(match self.active_view {
                        MainView::Chat => self.render_chat_view(cx).into_any_element(),
                        MainView::Settings => self.render_settings_view(cx).into_any_element(),
                    })
                    .when(
                        self.active_view == MainView::Chat && self.show_right_panel,
                        |el| el.child(self.render_right_panel(cx)),
                    ),
            )
    }
}

/// User text is typed as plain text: keep its single line breaks, which
/// markdown would otherwise fold into one paragraph (fenced code is left as is).
fn user_markdown(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    let mut in_fence = false;
    let lines: Vec<&str> = text.split('\n').collect();
    for (i, line) in lines.iter().enumerate() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
        out.push_str(line);
        if i + 1 < lines.len() {
            if !in_fence && !line.trim().is_empty() && !line.trim_start().starts_with("```") {
                out.push_str("  ");
            }
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_markdown_keeps_line_breaks_outside_fences() {
        assert_eq!(user_markdown("a\nb"), "a  \nb");
        assert_eq!(user_markdown("```\nx\ny\n```\nz"), "```\nx\ny\n```\nz");
        assert_eq!(user_markdown("a\n\nb"), "a  \n\nb");
    }

    #[test]
    fn split_error_separates_trailing_error() {
        let failed = format!("partial{ERROR_MARKER}boom");
        assert_eq!(split_error(&failed), ("partial", Some("boom")));
        let only = format!("{ERROR_MARKER}boom");
        assert_eq!(split_error(&only), ("", Some("boom")));
        // An ordinary reply that talks about errors is left alone.
        let normal = "Fixed it.\n\nError: auth token expired was the cause.";
        assert_eq!(split_error(normal), (normal, None));
    }
}
