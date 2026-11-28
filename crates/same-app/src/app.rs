mod approvals;
mod parallel;
mod skills;
mod token_stats;
mod usage;

use crate::i18n::t;
use crate::theme::{
    accent, accent_foreground, accent_indicator, accent_text, apply_gpui_theme, default_font_size,
    default_mono_font, default_ui_font, palette, status_amber, status_green, status_red,
};
use crate::updater::{self, ReleaseInfo, UpdateCheckOutcome, CURRENT_VERSION};
use crate::workspace_fs::{
    load_git_info, open_path_in_file_manager, reveal_in_file_manager, scan_directory, FileEntry,
    GitInfo,
};
use chrono::{DateTime, Local, Utc};
use gpui::{
    actions, div, list, prelude::FluentBuilder as _, px, AnyElement, App, AppContext, ClickEvent,
    ClipboardEntry, ClipboardItem, Context, Div, ElementId, Entity, ExternalPaths, FocusHandle,
    Focusable, FollowMode, FontWeight, Hsla, InteractiveElement, IntoElement, KeyBinding,
    ListAlignment, ListState, ObjectFit, ParentElement, PathPromptOptions, Pixels, Render,
    SharedString, Stateful, StatefulInteractiveElement, Styled, StyledImage as _, Subscription,
    Window,
};
use gpui_component::button::{Button, ButtonRounded, ButtonVariants as _};
use gpui_component::input::{
    Input, InputEvent, InputState, MoveDown, MoveUp, Textarea, TextareaState,
};
use gpui_component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenuItem};
use gpui_component::notification::Notification;
use gpui_component::scroll::{Scrollbar, ScrollbarAxis, ScrollbarMode};
use gpui_component::text::{RenderedText, TextView, TextViewState};
use gpui_component::Selectable;
use gpui_component::{
    ActiveTheme as _, Disableable as _, ElementExt as _, Icon, IconName, Sizable as _,
    StyledExt as _, Theme, TitleBar, WindowExt as _,
};
use same_agent::{
    exec as agent_exec, AgentManager, ExecPreference, InteractiveLoginEvent,
    InteractiveLoginHandle, PromptRequest, ProviderAuthStatus, ProviderCheckResult, ProviderSetup,
    SetupOutcome,
};
use same_core::paths::simplify_path;
use same_core::{
    AgentRun, AgentRunStatus, AppConfig, AppPaths, ApprovalMode, ColorTheme, ContextEnvironment,
    ContextManager, EventBus, ExecMode, HandoffMode, HandoffRequest, Language, Message,
    MessageRole, ModelInfo, PermissionRequest, Project, ProviderConfig, ProviderKind,
    ProviderSession, ProviderSessionStatus, Quest, SameEvent, Session, SessionStatus,
    SessionWorkdir, SlashCommandInfo, Storage, SubmitKey, ThemeMode, ToolCallRecord,
    ToolCallStatus, WorkdirMode, Workspace, MAX_CONCURRENT_SESSIONS, MAX_FONT_SIZE,
    MIN_CONCURRENT_SESSIONS, MIN_FONT_SIZE,
};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

use approvals::{split_plan, PendingInput, PLAN_MARKER};
use token_stats::StatsEntry;
use usage::QuotaEntry;

/// Removes the `📎 \`name\`` line that [`SameAppView::send_current_prompt`]
/// appends to a stored prompt with attachments.
fn strip_attachment_note(content: &str) -> &str {
    match content.rsplit_once("\n\n") {
        Some((text, last)) if last.starts_with("📎 `") => text,
        _ if content.starts_with("📎 `") => "",
        _ => content,
    }
}

/// The file name shown for an attachment.
fn attachment_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string())
}
use uuid::Uuid;

actions!(
    same,
    [
        NewSession,
        SaveFile,
        FocusPrompt,
        ToggleRightPanel,
        SendPrompt
    ]
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

const TURN_ANCHOR_TOP_GAP: Pixels = px(16.0);
const FEED_ITEM_BOTTOM_PAD: Pixels = px(16.0);

#[derive(Clone, Debug)]
struct FeedTurnAnchor {
    session_id: String,
    user_msg_id: String,
    estimated_user_height: Pixels,
    measured_user_height: Option<Pixels>,
    measured_assistant_height: Option<Pixels>,
}

/// Rough initial height of a newly sent user bubble row (including feed padding)
/// before GPUI's first list layout pass measures its real bounds.
fn estimate_user_row_height(content: &str, viewport_height: Pixels) -> Pixels {
    let approx_lines = content
        .lines()
        .map(|line| (line.chars().count() / 48) + 1)
        .sum::<usize>()
        .max(1);
    let raw = px(56.0 + approx_lines as f32 * 22.0);
    raw.min((viewport_height * 0.65).max(px(64.0)))
}

/// Minimum height to reserve on the trailing live item so a newly sent prompt
/// stays near the top of the viewport until the reply fills the space below.
fn compute_turn_tail_min_height(
    viewport_height: Pixels,
    user_row_height: Pixels,
    finalized_assistant_height: Option<Pixels>,
) -> Pixels {
    if viewport_height <= px(0.0) {
        return px(0.0);
    }
    (viewport_height
        - user_row_height
        - finalized_assistant_height.unwrap_or(px(0.0))
        - TURN_ANCHOR_TOP_GAP
        - FEED_ITEM_BOTTOM_PAD)
        .max(px(0.0))
}

const MAX_BODY_MARKER_LEN: usize = if ERROR_MARKER.len() > PLAN_MARKER.len() {
    ERROR_MARKER.len()
} else {
    PLAN_MARKER.len()
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LiveBodyMarkerState {
    /// Neither marker has appeared yet; the full `content` is the markdown body.
    Unmarked,
    /// `PLAN_MARKER` appeared (trailing newline at `err_start`), leaving
    /// `body_len` bytes before it. Still tracking whether only whitespace
    /// separates `PLAN_MARKER` from an `ERROR_MARKER` (which would cause
    /// `split_error`'s `trim_end` to strip `PLAN_MARKER`'s trailing newline).
    PlanSeen {
        err_start: usize,
        body_len: usize,
        ws_scan_pos: usize,
        err_candidate: Option<usize>,
    },
    /// The markdown body prefix length is permanently fixed at `usize` bytes.
    Frozen(usize),
}

struct LiveDraft {
    id: String,
    content: String,
    body_marker: LiveBodyMarkerState,
    thinking: String,
    tool_calls: Vec<ToolCallRecord>,
    /// The plan the agent proposed (plan mode).
    plan: Option<String>,
    /// Execution context label (`exec_label()`) when this turn was started,
    /// so turn failures never mutate another environment's provider check.
    exec_context: String,
}

impl LiveDraft {
    fn new() -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            content: String::new(),
            body_marker: LiveBodyMarkerState::Unmarked,
            thinking: String::new(),
            tool_calls: Vec::new(),
            plan: None,
            exec_context: String::new(),
        }
    }

    fn push_content(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let old_len = self.content.len();
        self.content.push_str(text);
        match self.body_marker {
            LiveBodyMarkerState::Frozen(_) => {}
            LiveBodyMarkerState::PlanSeen { .. } => self.advance_plan_seen(),
            LiveBodyMarkerState::Unmarked => {
                let mut scan_from = old_len.saturating_sub(MAX_BODY_MARKER_LEN - 1);
                while scan_from > 0 && !self.content.is_char_boundary(scan_from) {
                    scan_from -= 1;
                }
                let window = &self.content[scan_from..];
                if let Some(rel_err) = window.find(ERROR_MARKER) {
                    let err_pos = scan_from + rel_err;
                    let before_err = self.content[..err_pos].trim_end();
                    let plan_scan_from = scan_from.min(before_err.len());
                    let body_len = match before_err[plan_scan_from..].find(PLAN_MARKER) {
                        Some(rel_plan) => before_err[..plan_scan_from + rel_plan].trim_end().len(),
                        None => before_err.len(),
                    };
                    self.body_marker = LiveBodyMarkerState::Frozen(body_len);
                } else if let Some(rel_plan) = window.find(PLAN_MARKER) {
                    let plan_pos = scan_from + rel_plan;
                    let body_len = self.content[..plan_pos].trim_end().len();
                    let err_start = plan_pos + PLAN_MARKER.len() - 1;
                    self.body_marker = LiveBodyMarkerState::PlanSeen {
                        err_start,
                        body_len,
                        ws_scan_pos: plan_pos + PLAN_MARKER.len(),
                        err_candidate: None,
                    };
                    self.advance_plan_seen();
                }
            }
        }
    }

    fn advance_plan_seen(&mut self) {
        let LiveBodyMarkerState::PlanSeen {
            err_start,
            body_len,
            mut ws_scan_pos,
            mut err_candidate,
        } = self.body_marker
        else {
            return;
        };

        if err_candidate.is_none() {
            if let Some((rel, _)) = self.content[ws_scan_pos..]
                .char_indices()
                .find(|(_, ch)| !ch.is_whitespace())
            {
                let non_ws_pos = ws_scan_pos + rel;
                ws_scan_pos = non_ws_pos;
                // `ERROR_MARKER` begins with ASCII `"\n\n"`, so checking raw
                // bytes first guarantees `non_ws_pos - 2` is a UTF-8 char
                // boundary even if preceded by multi-byte Unicode whitespace.
                if non_ws_pos >= err_start + 2
                    && self.content.as_bytes()[non_ws_pos - 2..non_ws_pos] == *b"\n\n"
                {
                    err_candidate = Some(non_ws_pos - 2);
                } else {
                    self.body_marker = LiveBodyMarkerState::Frozen(body_len);
                    return;
                }
            } else {
                self.body_marker = LiveBodyMarkerState::PlanSeen {
                    err_start,
                    body_len,
                    ws_scan_pos: self.content.len(),
                    err_candidate: None,
                };
                return;
            }
        }

        if let Some(candidate) = err_candidate {
            let tail = &self.content[candidate..];
            if tail.len() >= ERROR_MARKER.len() {
                if tail.starts_with(ERROR_MARKER) {
                    let trimmed = self.content[..candidate].trim_end().len();
                    self.body_marker = LiveBodyMarkerState::Frozen(trimmed);
                } else {
                    self.body_marker = LiveBodyMarkerState::Frozen(body_len);
                }
            } else if !ERROR_MARKER.starts_with(tail) {
                self.body_marker = LiveBodyMarkerState::Frozen(body_len);
            } else {
                self.body_marker = LiveBodyMarkerState::PlanSeen {
                    err_start,
                    body_len,
                    ws_scan_pos,
                    err_candidate: Some(candidate),
                };
            }
        }
    }

    fn markdown_body(&self) -> &str {
        match self.body_marker {
            LiveBodyMarkerState::Unmarked => &self.content,
            LiveBodyMarkerState::PlanSeen { body_len, .. }
            | LiveBodyMarkerState::Frozen(body_len) => &self.content[..body_len],
        }
    }
}

impl Default for LiveDraft {
    fn default() -> Self {
        Self::new()
    }
}

/// Maximum number of message `TextViewState` instances retained in the active
/// session so manual scrolling through history reuses parsed Markdown without
/// dropping state on virtualization or growing memory without bound.
const MAX_SESSION_MARKDOWN_VIEWS: usize = 128;
/// Conservative minimum-row height hint for unmeasured feed items so scrollbar
/// geometry and upward scrolling stay stable without overestimating short rows
/// or forcing `ListState::measure_all()`.
const FEED_UNMEASURED_ITEM_HEIGHT_HINT: Pixels = px(56.0);
/// Number of recent tool calls rendered per page in the unfiltered right-pane Tools view.
const TOOLS_HISTORY_PAGE_SIZE: usize = 30;

struct LiveMarkdownView {
    session_id: String,
    draft_id: String,
    synced_len: usize,
    state: Entity<TextViewState>,
    last_rendered: RenderedText,
    _subscription: Subscription,
}

struct FinalizedMarkdownView {
    msg_id: String,
    state: Entity<TextViewState>,
    /// Whether `state` has committed its parsed document (immediately true for
    /// synchronous parses `<= 4 KiB`, or flipped true when async parse finishes).
    ready: bool,
    /// Observer for an initial async parse when opening a `> 4 KiB` message
    /// from history; cleared once committed.
    _pending_parse: Option<Subscription>,
    /// Full-document parse and its observer warming up in the background for
    /// replies larger than `gpui-base`'s synchronous threshold; swapped into
    /// `state` once committed.
    _pending_full: Option<(Entity<TextViewState>, Subscription)>,
}

#[derive(Debug, PartialEq, Eq)]
enum MarkdownBodySync<'a> {
    Unchanged,
    Append(&'a str),
    Replace(&'a str),
}

fn message_markdown_body(content: &str) -> &str {
    let (body, _) = split_error(content);
    let (body, _) = split_plan(body);
    body
}

fn diff_markdown_body(body: &str, synced_len: usize) -> MarkdownBodySync<'_> {
    if body.len() > synced_len && body.is_char_boundary(synced_len) {
        MarkdownBodySync::Append(&body[synced_len..])
    } else if body.len() != synced_len {
        MarkdownBodySync::Replace(body)
    } else {
        MarkdownBodySync::Unchanged
    }
}

fn apply_markdown_body_sync(
    view: &mut LiveMarkdownView,
    body: &str,
    cx: &mut Context<SameAppView>,
) {
    match diff_markdown_body(body, view.synced_len) {
        MarkdownBodySync::Unchanged => {}
        MarkdownBodySync::Append(delta) => {
            let state = view.state.clone();
            state.update(cx, |s, cx| s.push_str(delta, cx));
            view.synced_len = body.len();
            view.last_rendered = state.read(cx).rendered_text();
        }
        MarkdownBodySync::Replace(full) => {
            let state = view.state.clone();
            state.update(cx, |s, cx| s.set_text(full, cx));
            view.synced_len = body.len();
            view.last_rendered = state.read(cx).rendered_text();
        }
    }
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
    LoginOutput {
        attempt_id: u64,
        provider_id: String,
        chunk: String,
    },
    LoginDone {
        attempt_id: u64,
        provider_id: String,
        result: Result<String, String>,
    },
    ModelsDiscovered {
        provider_id: String,
        target: String,
        result: Result<same_agent::discovery::Discovered, String>,
        /// Started by the user (show the outcome), not in the background.
        manual: bool,
    },
    UpdateChecked {
        result: Result<UpdateCheckOutcome, String>,
        manual: bool,
    },
    UpdateInstalled {
        version: String,
        result: Result<(), String>,
    },
    QuotaFetched {
        provider_id: String,
        generation: u64,
        /// The exec label of the project the fetch was started for.
        context: String,
        /// Where the agent ran.
        target: String,
        result: Result<same_core::QuotaReport, String>,
    },
    StatsFetched {
        provider_id: String,
        days: u32,
        context: String,
        target: String,
        result: Result<same_core::StatsReport, String>,
    },
    ExternalSkillsDiscovered {
        generation: u64,
        skills: Vec<same_core::ExternalSkill>,
    },
    SkillsSynced {
        result: Result<usize, String>,
        manual: bool,
    },
    SkillDraftGenerated {
        request_id: u64,
        result: Result<same_agent::GeneratedSkillDraft, String>,
    },
    WorkspaceFilesLoaded {
        generation: u64,
        root: PathBuf,
        entries: Vec<FileEntry>,
    },
    WorkspaceGitLoaded {
        generation: u64,
        root: PathBuf,
        git_info: GitInfo,
        worktrees: Option<Vec<same_core::WorktreeInfo>>,
    },
}

enum UpdateState {
    Idle,
    Checking,
    UpToDate,
    Available(ReleaseInfo),
    Downloading(ReleaseInfo),
    Installed(String),
    Error(String),
}

/// File (in the data dir) that keeps the last model list of each provider,
/// so the picker is complete right after launch.
const MODELS_CACHE_FILE: &str = "models_cache.json";

#[derive(PartialEq, Eq, Clone, Copy)]
enum MainView {
    Chat,
    Skills,
    Usage,
    Settings,
}

/// A page of the settings screen.
#[derive(PartialEq, Eq, Clone, Copy)]
enum SettingsSection {
    General,
    Appearance,
    Providers,
    About,
}

impl SettingsSection {
    const ALL: [SettingsSection; 4] = [
        SettingsSection::General,
        SettingsSection::Appearance,
        SettingsSection::Providers,
        SettingsSection::About,
    ];

    fn title(self, lang: Language) -> &'static str {
        match self {
            SettingsSection::General => t(lang, "一般", "General"),
            SettingsSection::Appearance => t(lang, "外観", "Appearance"),
            SettingsSection::Providers => t(lang, "プロバイダ接続", "Providers"),
            SettingsSection::About => t(lang, "SAME について", "About"),
        }
    }

    fn icon(self) -> IconName {
        match self {
            SettingsSection::General => IconName::Settings2,
            SettingsSection::Appearance => IconName::Palette,
            SettingsSection::Providers => IconName::Bot,
            SettingsSection::About => IconName::Info,
        }
    }
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum RightTab {
    Files,
    Git,
    Tools,
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
    /// Observed `TextViewState` for the selected session's streaming markdown body,
    /// so async markdown parses invalidate the trailing feed row height without
    /// allocating parser state for unselected background sessions.
    live_markdown_view: Option<LiveMarkdownView>,
    /// Bounded handoff cache of recently finalized `TextViewState` instances for the
    /// active session, avoiding reparse flicker on finalize and catching late parses.
    session_markdown_views: VecDeque<FinalizedMarkdownView>,
    pending_permissions: Vec<PermissionRequest>,
    /// Question cards waiting for answers.
    pending_inputs: Vec<PendingInput>,
    /// Focus of the first open permission / question card of the selected
    /// session, so it can be answered from the keyboard.
    approval_focus: FocusHandle,
    /// Move the focus to that card at the next render.
    focus_approval: bool,
    /// The window is in the foreground (no OS notifications then).
    window_active: bool,
    /// Per session: the approval mode before it switched to plan, restored
    /// when the plan is implemented.
    pre_plan_mode: HashMap<String, ApprovalMode>,
    slash_commands: HashMap<String, Vec<SlashCommandInfo>>,
    /// Models an agent reported (ACP), by provider id, so a fresh session
    /// can offer them before its first prompt.
    agent_models: HashMap<String, Vec<ModelInfo>>,
    /// (provider id, exec target) pairs whose models were asked for this run.
    models_discovered: HashSet<(String, String)>,
    /// Providers with a model discovery in flight.
    models_discovering: HashSet<String>,
    /// The last usage-limit fetch of each provider, by provider id.
    quotas: HashMap<String, QuotaEntry>,
    /// Providers with a usage-limit fetch in flight.
    quotas_fetching: HashSet<String>,
    /// Per-provider generation counter to discard in-flight quota fetches
    /// started before a login or provider config change.
    quota_generations: HashMap<String, u64>,
    /// The last token-history read of each provider, by provider id.
    stats: HashMap<String, StatsEntry>,
    /// Providers with a token-history read in flight.
    stats_fetching: HashSet<String>,
    /// The period of the token statistics, in days (1 = today).
    stats_days: u32,
    /// Count prompt-cache tokens in the token statistics.
    stats_include_cache: bool,
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
    /// Active in-app interactive login session (one at a time so the inline
    /// code input always routes to the right process).
    active_login: Option<(u64, String, InteractiveLoginHandle)>,
    next_login_attempt_id: u64,
    login_code_input: Entity<InputState>,
    expanded_thinking: HashSet<String>,
    expanded_tools: HashSet<String>,
    logs: Vec<String>,
    pending_notes: Vec<(NoteKind, String)>,
    /// Two-step delete: the key of the item whose delete button was clicked
    /// once and now asks for confirmation (e.g. "sess:<id>").
    confirm_delete: Option<String>,
    update_state: UpdateState,

    // Chat
    prompt_input: Entity<TextareaState>,
    prompt_empty: bool,
    /// Files attached to the next prompt (host paths).
    attachments: Vec<PathBuf>,
    /// Skills explicitly attached to the next prompt via the `+` menu.
    selected_prompt_skills: Vec<skills::PromptSkillRef>,
    /// Whether the searchable skill picker popover is open above the composer.
    show_skill_picker: bool,
    skill_picker_search: Entity<InputState>,
    /// Temp image files created by pasting into the current draft; deleted if
    /// removed or cleared before sending.
    pasted_attachments: HashSet<PathBuf>,
    /// The earlier prompt recalled with Up: (index from the newest, its
    /// text). Browsing continues only while the input still shows it.
    history_recall: Option<(usize, String)>,
    /// The prompt is a `/command` being typed: show suggestions.
    prompt_is_command: bool,
    /// The prompt ends with a `$skill` token being typed: show skill suggestions.
    prompt_is_skill_mention: bool,
    /// Virtualized chat feed: one item per stored message, plus a trailing
    /// item for the live reply / permission prompts / empty state. Only the
    /// items on screen are laid out, so long sessions stay cheap to render.
    feed: ListState,
    /// (session id, item count) the feed was last synced with.
    feed_synced: std::cell::RefCell<(Option<String>, usize)>,
    /// Whether the trailing item's rendered height needs remeasurement next frame.
    feed_tail_dirty: std::cell::Cell<bool>,
    /// Minimum height reserved for the trailing item to keep a newly sent user prompt near the top.
    feed_tail_min_height: std::cell::Cell<Pixels>,
    /// Most recent prepainted height of the live reply content inside the trailing item.
    feed_live_content_height: std::rc::Rc<std::cell::Cell<Pixels>>,
    /// Active turn anchor keeping the newly sent user prompt near the viewport top.
    feed_turn_anchor: std::cell::RefCell<Option<FeedTurnAnchor>>,
    renaming_session_id: Option<String>,
    rename_input: Entity<InputState>,

    // Right panel
    show_right_panel: bool,
    right_tab: RightTab,
    right_split: bool,
    right_bottom_tab: RightTab,
    /// Optional turn filter (`Message::id` / `LiveDraft::id`) for the right-pane Tools view.
    selected_tool_turn_id: Option<String>,
    /// Maximum number of recent tool calls shown in the unfiltered Tools view.
    tools_visible_limit: usize,
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
    // Skills Manager
    skills_ui: skills::SkillsUiState,
    /// Virtualized rows of the font dropdown, one per `font_rows` entry.
    /// Shared by both pickers (only one is open at a time).
    font_list: ListState,
    /// The dropdown's rows after filtering: `None` is the platform default,
    /// `Some(i)` is `font_names[i]`.
    font_rows: Vec<Option<usize>>,
    /// Search box at the top of the font dropdown.
    font_search: Entity<InputState>,

    // Parallel sessions, Context Handoff, and Workdir/Worktree
    session_ui_states: HashMap<String, parallel::SessionUiState>,
    /// Active run tracking per session (captures the turn's exec_mode and workdir).
    active_runs: HashMap<String, parallel::ActiveRunInfo>,
    background_alerts: Vec<parallel::BackgroundAlert>,
    pending_provider_switch: Option<parallel::PendingProviderSwitch>,
    pending_worktree_confirm: Option<parallel::PendingWorktreeConfirm>,
    session_workdirs: HashMap<String, SessionWorkdir>,
    cached_worktrees: Vec<same_core::WorktreeInfo>,
    /// Cached metadata for running/waiting sessions (avoids SQLite lookups during render).
    active_session_meta: HashMap<String, Session>,
    /// Generation counters for discarding stale background workspace scans / git loads.
    workspace_files_generation: u64,
    workspace_git_generation: u64,
    /// Caps concurrent background directory scans to at most one in flight.
    workspace_files_in_flight: bool,
    /// When true, triggers one trailing scan with the latest filter once the current scan finishes.
    workspace_files_pending_refresh: bool,
    /// Keeps an in-flight worktree list refresh alive even if a Git-only refresh supersedes it.
    worktrees_refresh_pending: bool,
    /// Effective workdir root currently loaded in `file_entries` / `git_info`.
    loaded_workspace_root: Option<PathBuf>,
    /// Prevents `file_filter_input` clearing during project switch from triggering a duplicate scan.
    suppress_file_filter_reload: bool,
}

impl Drop for SameAppView {
    fn drop(&mut self) {
        self.clear_attachments();
        self.selected_prompt_skills.clear();
    }
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

        // Pump async messages into GPUI view state without polling.
        // Drain all queued messages in one update pass to coalesce rapid
        // streaming deltas and avoid redundant full-view re-renders.
        cx.spawn(async move |this, cx| {
            while let Some(first) = ui_rx.recv().await {
                let mut batch = vec![first];
                while let Ok(next) = ui_rx.try_recv() {
                    batch.push(next);
                }
                let updated = this.update(cx, |view, cx| {
                    let mut needs_notify = false;
                    let mut live_md_dirty = false;
                    for msg in batch {
                        let (notify, md_dirty) = view.handle_async_msg(msg, cx);
                        needs_notify |= notify;
                        live_md_dirty |= md_dirty;
                    }
                    if live_md_dirty {
                        view.sync_live_markdown_for_selected(cx);
                    }
                    if needs_notify {
                        cx.notify();
                    }
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
        let login_code_input = cx.new(|cx| InputState::new(window, cx));

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
                    if matches!(ev, InputEvent::Change) && !this.suppress_file_filter_reload {
                        this.reload_workspace_files_only(cx);
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
            cx.subscribe_in(
                &login_code_input,
                window,
                |this, _, ev: &InputEvent, window, cx| {
                    if matches!(ev, InputEvent::PressEnter { .. }) {
                        this.submit_in_app_login_code(window, cx);
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

        // OS notifications only while the window is in the background.
        subscriptions.push(cx.observe_window_activation(window, |this, window, _| {
            this.window_active = window.is_window_active();
        }));

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

        let skills_ui = skills::SkillsUiState::new(
            paths.data_dir.clone(),
            storage.clone(),
            events.clone(),
            window,
            cx,
        );
        subscriptions.push(cx.subscribe_in(
            &skills_ui.editor_input,
            window,
            |this, _, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::Change) {
                    let cur = this.skills_ui.editor_input.read(cx).value().to_string();
                    this.skills_ui.editor_dirty = cur != this.skills_ui.editor_saved;
                    cx.notify();
                }
            },
        ));
        let skill_picker_search =
            cx.new(|cx| InputState::new(window, cx).placeholder("スキル名・説明でフィルタ..."));
        subscriptions.push(cx.subscribe_in(
            &skill_picker_search,
            window,
            |_, _, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::Change) {
                    cx.notify();
                }
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &skills_ui.search_input,
            window,
            |_, _, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::Change) {
                    cx.notify();
                }
            },
        ));

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
            live_markdown_view: None,
            session_markdown_views: VecDeque::new(),
            pending_permissions: vec![],
            pending_inputs: vec![],
            approval_focus: cx.focus_handle(),
            focus_approval: false,
            window_active: true,
            pre_plan_mode: HashMap::new(),
            slash_commands: HashMap::new(),
            agent_models,
            models_discovered: HashSet::new(),
            models_discovering: HashSet::new(),
            quotas: HashMap::new(),
            quotas_fetching: HashSet::new(),
            quota_generations: HashMap::new(),
            stats: HashMap::new(),
            stats_fetching: HashSet::new(),
            stats_days: token_stats::DEFAULT_STATS_DAYS,
            stats_include_cache: true,
            agent_default_model: HashMap::new(),
            agent_current_model: HashMap::new(),
            provider_checks: HashMap::new(),
            check_generation: 0,
            connect_results: HashMap::new(),
            login_results: HashMap::new(),
            setup_running: HashSet::new(),
            active_login: None,
            next_login_attempt_id: 0,
            login_code_input,
            expanded_thinking: HashSet::new(),
            expanded_tools: HashSet::new(),
            logs: vec![],
            pending_notes: vec![],
            confirm_delete: None,
            update_state: UpdateState::Idle,
            prompt_input,
            prompt_empty: true,
            attachments: Vec::new(),
            selected_prompt_skills: Vec::new(),
            show_skill_picker: false,
            skill_picker_search,
            pasted_attachments: HashSet::new(),
            history_recall: None,
            prompt_is_command: false,
            prompt_is_skill_mention: false,
            feed: {
                let feed = ListState::new(0, ListAlignment::Top, px(1000.0));
                feed.set_follow_mode(FollowMode::Tail);
                let weak_view = cx.entity().downgrade();
                feed.set_item_measurement_readiness(move |ix, cx| {
                    weak_view
                        .read_with(cx, |this, _| this.is_feed_item_measurement_ready(ix))
                        .unwrap_or(true)
                });
                feed
            },
            feed_synced: Default::default(),
            feed_tail_dirty: std::cell::Cell::new(false),
            feed_tail_min_height: std::cell::Cell::new(px(0.0)),
            feed_live_content_height: std::rc::Rc::new(std::cell::Cell::new(px(0.0))),
            feed_turn_anchor: std::cell::RefCell::new(None),
            renaming_session_id: None,
            rename_input,
            show_right_panel: true,
            right_tab: RightTab::Files,
            right_split: false,
            right_bottom_tab: RightTab::Tools,
            selected_tool_turn_id: None,
            tools_visible_limit: TOOLS_HISTORY_PAGE_SIZE,
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
            skills_ui,
            session_ui_states: HashMap::new(),
            active_runs: HashMap::new(),
            background_alerts: Vec::new(),
            pending_provider_switch: None,
            pending_worktree_confirm: None,
            session_workdirs: HashMap::new(),
            cached_worktrees: Vec::new(),
            active_session_meta: HashMap::new(),
            workspace_files_generation: 0,
            workspace_git_generation: 0,
            workspace_files_in_flight: false,
            workspace_files_pending_refresh: false,
            worktrees_refresh_pending: false,
            loaded_workspace_root: None,
            suppress_file_filter_reload: false,
        };
        app.agent_manager
            .set_max_concurrent_sessions(app.config.max_concurrent_sessions());

        if app.config.migrated_auto_approve {
            // Saved without the old key from now on.
            app.save_config();
            app.note(
                NoteKind::Info,
                t(
                    app.config.language,
                    "「自動承認」は承認モード「編集は自動」に置き換わりました（入力欄の下で変更できます）",
                    "Auto-approve became the \"Accept edits\" approval mode (change it under the prompt)",
                ),
            );
        }
        app.apply_placeholders(window, cx);
        app.reload_project_state(window, cx);
        app.reload_skills_state();
        app.trigger_external_skill_discovery();
        app.refresh_provider_checks();
        app.check_for_updates(false);
        // Keeps the usage limits (title bar rings, dashboard) current.
        app.refresh_quotas(Duration::ZERO, None);
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(usage::QUOTA_TICK).await;
            let alive = this.update(cx, |view, cx| {
                view.tick_quotas();
                cx.notify();
            });
            if alive.is_err() {
                break;
            }
        })
        .detach();
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
        set_input(
            &self.login_code_input,
            t(
                lang,
                "認証コード または リダイレクト先 URL を貼り付け",
                "Paste authorization code or redirect URL",
            ),
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

    fn is_selected_session(&self, session_id: &str) -> bool {
        self.selected_session_id.as_deref() == Some(session_id)
    }

    pub(super) fn mark_feed_tail_dirty(&self) {
        self.feed_tail_dirty.set(true);
    }

    fn sync_live_markdown_for_selected(&mut self, cx: &mut Context<Self>) {
        let Some(sid) = self.selected_session_id.as_deref() else {
            self.live_markdown_view = None;
            return;
        };
        let Some(draft) = self.live_drafts.get(sid) else {
            self.live_markdown_view = None;
            return;
        };
        let body = draft.markdown_body();
        if let Some(view) = self.live_markdown_view.as_mut() {
            if view.session_id == sid && view.draft_id == draft.id {
                if body.len() < view.synced_len && body.trim().is_empty() {
                    self.live_markdown_view = None;
                    return;
                }
                apply_markdown_body_sync(view, body, cx);
                return;
            }
        }
        if body.trim().is_empty() {
            self.live_markdown_view = None;
            return;
        }

        let sid_owned = sid.to_string();
        let draft_id = draft.id.clone();
        let synced_len = body.len();
        let state = cx.new(|cx| TextViewState::markdown(body, cx));
        let last_rendered = state.read(cx).rendered_text();
        let obs_sid = sid_owned.clone();
        let obs_msg_id = draft_id.clone();
        let subscription = cx.observe(&state, move |this, state, cx| {
            let rendered = state.read(cx).rendered_text();
            if let Some(view) = this
                .live_markdown_view
                .as_mut()
                .filter(|v| v.session_id == obs_sid && v.draft_id == obs_msg_id)
            {
                if view.last_rendered != rendered {
                    view.last_rendered = rendered;
                    if this.is_selected_session(&obs_sid) {
                        this.mark_feed_tail_dirty();
                        cx.notify();
                    }
                }
            }
        });
        self.live_markdown_view = Some(LiveMarkdownView {
            session_id: sid_owned,
            draft_id,
            synced_len,
            state,
            last_rendered,
            _subscription: subscription,
        });
    }

    fn handoff_finalized_markdown(
        &mut self,
        session_id: &str,
        msg_id: &str,
        final_body: &str,
        live_md: LiveMarkdownView,
        cx: &mut Context<Self>,
    ) {
        if live_md.draft_id != msg_id || final_body.trim().is_empty() {
            return;
        }
        let full_state = cx.new(|cx| TextViewState::markdown(final_body, cx));
        let initial_rendered = full_state.read(cx).rendered_text();
        if initial_rendered.source().len() == final_body.len() {
            // Replies within gpui-base's synchronous threshold parse completely
            // right away; swap immediately so forward references resolve without
            // keeping the incremental streaming state.
            self.push_session_markdown_view(FinalizedMarkdownView {
                msg_id: msg_id.to_string(),
                state: full_state,
                ready: true,
                _pending_parse: None,
                _pending_full: None,
            });
            return;
        }

        // Larger replies parse asynchronously in the background. Keep rendering
        // the streamed state until the full-document parse commits so handoff
        // does not flash blank, then swap atomically and remeasure the row.
        let obs_sid = session_id.to_string();
        let obs_msg_id = msg_id.to_string();
        let subscription = cx.observe(&full_state, move |this, full_state, cx| {
            let rendered = full_state.read(cx).rendered_text();
            if rendered == initial_rendered {
                return;
            }
            if let Some(view) = this
                .session_markdown_views
                .iter_mut()
                .find(|v| v.msg_id == obs_msg_id)
            {
                view.state = full_state;
                view.ready = true;
                view._pending_full = None;
                if this.is_selected_session(&obs_sid) {
                    if let Some(ix) = this.messages.iter().rposition(|m| m.id == obs_msg_id) {
                        this.remeasure_feed_item(ix);
                        cx.notify();
                    }
                }
            }
        });
        self.push_session_markdown_view(FinalizedMarkdownView {
            msg_id: msg_id.to_string(),
            state: live_md.state,
            ready: true,
            _pending_parse: None,
            _pending_full: Some((full_state, subscription)),
        });
    }

    fn ensure_session_markdown_state_for_stored_message(
        &mut self,
        ix: usize,
        cx: &mut Context<Self>,
    ) -> Option<Entity<TextViewState>> {
        let msg = self.messages.get(ix)?;
        if let Some(pos) = self
            .session_markdown_views
            .iter()
            .position(|v| v.msg_id == msg.id)
        {
            if let Some(view) = self.session_markdown_views.remove(pos) {
                let state = view.state.clone();
                self.session_markdown_views.push_back(view);
                return Some(state);
            }
        }

        let (session_id, msg_id, body) = if msg.role == MessageRole::User {
            (
                msg.session_id.clone(),
                msg.id.clone(),
                user_markdown(&msg.content),
            )
        } else {
            let body = message_markdown_body(&msg.content);
            if body.trim().is_empty() {
                return None;
            }
            (msg.session_id.clone(), msg.id.clone(), body.to_string())
        };
        Some(self.ensure_session_markdown_state(&session_id, &msg_id, &body, cx))
    }

    fn ensure_session_markdown_state(
        &mut self,
        session_id: &str,
        msg_id: &str,
        body: &str,
        cx: &mut Context<Self>,
    ) -> Entity<TextViewState> {
        if let Some(pos) = self
            .session_markdown_views
            .iter()
            .position(|v| v.msg_id == msg_id)
        {
            if let Some(view) = self.session_markdown_views.remove(pos) {
                let state = view.state.clone();
                self.session_markdown_views.push_back(view);
                return state;
            }
        }

        let state = cx.new(|cx| TextViewState::markdown(body, cx));
        let initial_rendered = state.read(cx).rendered_text();
        let ready = initial_rendered.source().len() == body.len();
        let pending_parse = if ready {
            None
        } else {
            let obs_sid = session_id.to_string();
            let obs_msg_id = msg_id.to_string();
            Some(cx.observe(&state, move |this, state, cx| {
                let rendered = state.read(cx).rendered_text();
                if rendered == initial_rendered {
                    return;
                }
                if let Some(view) = this
                    .session_markdown_views
                    .iter_mut()
                    .find(|v| v.msg_id == obs_msg_id)
                {
                    view.ready = true;
                    view._pending_parse = None;
                }
                if this.is_selected_session(&obs_sid) {
                    if let Some(ix) = this.messages.iter().rposition(|m| m.id == obs_msg_id) {
                        this.remeasure_feed_item(ix);
                        cx.notify();
                    }
                }
            }))
        };
        self.push_session_markdown_view(FinalizedMarkdownView {
            msg_id: msg_id.to_string(),
            state: state.clone(),
            ready,
            _pending_parse: pending_parse,
            _pending_full: None,
        });
        state
    }

    fn is_feed_item_measurement_ready(&self, ix: usize) -> bool {
        let Some(msg) = self.messages.get(ix) else {
            return true;
        };
        self.session_markdown_views
            .iter()
            .find(|v| v.msg_id == msg.id)
            .is_none_or(|v| v.ready)
    }

    fn push_session_markdown_view(&mut self, view: FinalizedMarkdownView) {
        self.session_markdown_views
            .retain(|existing| existing.msg_id != view.msg_id);
        while self.session_markdown_views.len() >= MAX_SESSION_MARKDOWN_VIEWS {
            self.session_markdown_views.pop_front();
        }
        self.session_markdown_views.push_back(view);
    }

    fn tracked_markdown_state(
        &self,
        msg: &Message,
        is_live: bool,
    ) -> Option<&Entity<TextViewState>> {
        if is_live {
            self.live_markdown_view
                .as_ref()
                .filter(|v| v.session_id == msg.session_id && v.draft_id == msg.id)
                .map(|v| &v.state)
        } else {
            self.session_markdown_views
                .iter()
                .find(|v| v.msg_id == msg.id)
                .map(|v| &v.state)
        }
    }

    fn clear_turn_anchor(&self) {
        *self.feed_turn_anchor.borrow_mut() = None;
        self.feed_live_content_height.set(px(0.0));
        if self.feed_tail_min_height.replace(px(0.0)) > px(0.0) {
            self.feed_tail_dirty.set(true);
        }
    }

    /// Jumps the feed to the latest message and keeps following new output
    /// (until the user scrolls up; scrolling back down resumes following).
    fn scroll_feed_to_end(&self) {
        self.feed.set_follow_mode(FollowMode::Tail);
        self.feed.scroll_to_end();
    }

    fn is_feed_scrolled_up(&self) -> bool {
        let max_y = self.feed.max_offset_for_scrollbar().y;
        if max_y <= px(0.0) || self.feed.is_following_tail() {
            return false;
        }
        let current_y = -self.feed.scroll_px_offset_for_scrollbar().y;
        current_y < max_y - px(2.0)
    }

    fn should_anchor_new_turn(&self) -> bool {
        if self.messages.is_empty() {
            return false;
        }
        let viewport = self.feed.viewport_bounds();
        if viewport.size.height <= px(0.0) {
            return false;
        }
        if self.feed.max_offset_for_scrollbar().y > px(0.0) {
            return true;
        }
        let last_ix = self.messages.len() - 1;
        self.feed
            .bounds_for_item(last_ix)
            .is_some_and(|b| b.bottom() - viewport.top() > viewport.size.height * 0.45)
    }

    fn sync_turn_anchor_tail_height(&self) -> (Pixels, bool) {
        let viewport = self.feed.viewport_bounds();
        let selected_sid = self.selected_session_id.as_deref();
        let synced_matches = self.feed_synced.borrow().0 == self.selected_session_id;
        let feed_count = self.feed.item_count();
        let mut min_h = px(0.0);
        let mut clear_stale = false;

        if let Some(anchor) = self.feed_turn_anchor.borrow_mut().as_mut() {
            if Some(anchor.session_id.as_str()) != selected_sid {
                clear_stale = true;
            } else if let Some(anchor_ix) = self
                .messages
                .iter()
                .rposition(|m| m.id == anchor.user_msg_id)
            {
                let running = self.is_session_running(&anchor.session_id);
                let has_finalized_reply = !running && anchor_ix + 1 < self.messages.len();
                if synced_matches {
                    if anchor_ix < feed_count {
                        if let Some(b) = self.feed.bounds_for_item(anchor_ix) {
                            anchor.measured_user_height = Some(b.size.height);
                        }
                    }
                    if has_finalized_reply && anchor_ix + 1 < feed_count {
                        if let Some(b) = self.feed.bounds_for_item(anchor_ix + 1) {
                            anchor.measured_assistant_height = Some(b.size.height);
                        }
                    }
                }
                let user_h = anchor
                    .measured_user_height
                    .unwrap_or(anchor.estimated_user_height);
                let assistant_h = if has_finalized_reply {
                    anchor.measured_assistant_height
                } else {
                    None
                };
                min_h = compute_turn_tail_min_height(viewport.size.height, user_h, assistant_h);
            } else {
                clear_stale = true;
            }
        }

        if clear_stale {
            *self.feed_turn_anchor.borrow_mut() = None;
        }

        let changed = (min_h - self.feed_tail_min_height.get()).abs() > px(0.5);
        if changed {
            self.feed_tail_min_height.set(min_h);
            self.feed_tail_dirty.set(true);
        }

        (min_h, changed)
    }

    /// Brings the feed's item count in line with the current session. Called
    /// on every render (and right after sending); cheap when nothing changed.
    fn sync_feed(&self) {
        let count = self.messages.len() + 1;
        let mut synced = self.feed_synced.borrow_mut();
        let session_changed = synced.0 != self.selected_session_id;
        let old = synced.1;
        if session_changed || count < old || old == 0 {
            self.feed
                .reset_with_uniform_height(count, FEED_UNMEASURED_ITEM_HEIGHT_HINT);
            self.scroll_feed_to_end();
            self.feed_tail_dirty.set(false);
        } else if count > old {
            // New messages: replace the old trailing item and append.
            self.feed.splice(old - 1..old, count - old + 1);
            self.feed_tail_dirty.set(false);
        } else if self.feed_tail_dirty.replace(false) && count > 0 {
            self.feed.remeasure_items(count - 1..count);
        }
        *synced = (self.selected_session_id.clone(), count);
    }

    /// Inserts the finalized assistant message before the trailing live item
    /// without resetting `scroll_anchor` on that item if the user scrolled there.
    fn sync_feed_after_finalize(&self) {
        let count = self.messages.len() + 1;
        let mut synced = self.feed_synced.borrow_mut();
        if synced.0 == self.selected_session_id && synced.1 > 0 && count == synced.1 + 1 {
            let old = synced.1;
            self.feed.splice(old..old, 1);
            self.feed.remeasure_items(old - 1..old);
            *synced = (self.selected_session_id.clone(), count);
            self.feed_tail_dirty.set(true);
        } else {
            drop(synced);
            self.mark_feed_tail_dirty();
            self.sync_feed();
        }
    }

    fn remeasure_feed_item(&self, ix: usize) {
        if ix < self.feed.item_count() {
            self.feed.remeasure_items(ix..ix + 1);
        }
    }

    /// Forces a full feed rebuild on the next render (messages replaced).
    fn invalidate_feed(&self) {
        *self.feed_synced.borrow_mut() = (None, 0);
    }

    fn select_session(&mut self, session_id: String, window: &mut Window, cx: &mut Context<Self>) {
        self.select_session_inner(session_id, false, window, cx);
    }

    fn select_session_inner(
        &mut self,
        session_id: String,
        known_empty: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let prev_sid = self.selected_session_id.clone();
        let switching = prev_sid.as_deref() != Some(session_id.as_str());
        if switching {
            self.commit_rename(cx);
            if let Some(old_sid) = prev_sid {
                self.save_session_ui_state(&old_sid, cx);
            }
            self.pending_provider_switch = None;
            self.pending_worktree_confirm = None;
        }
        self.selected_session_id = Some(session_id.clone());
        self.active_view = MainView::Chat;
        self.confirm_delete = None;
        if known_empty {
            self.clear_turn_anchor();
            self.session_markdown_views.clear();
            self.selected_tool_turn_id = None;
            self.tools_visible_limit = TOOLS_HISTORY_PAGE_SIZE;
            self.messages.clear();
            self.invalidate_feed();
            self.sync_live_markdown_for_selected(cx);
        } else {
            self.reload_messages(window, cx);
        }
        let saved_scroll = if switching {
            self.restore_session_ui_state(&session_id, window, cx)
        } else {
            None
        };
        if let Some(offset) = saved_scroll {
            self.sync_feed();
            self.feed.scroll_to(offset);
        } else {
            self.scroll_feed_to_end();
        }
        // Only reload workspace files & git info when the effective working
        // directory actually changed (same-workdir session switches are instant).
        let next_root = self.current_workspace_root();
        if next_root != self.loaded_workspace_root {
            self.reload_workspace_files(cx);
        }
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
                let default_mode = self.config.default_approval_mode;
                let _ = self
                    .storage
                    .set_session_approval_mode(&sess.id, Some(default_mode));
                sess.approval_mode = Some(default_mode);
                if effort.is_some() {
                    let _ = self.storage.set_session_effort(&sess.id, effort.as_deref());
                    sess.effort = effort;
                }
                let id = sess.id.clone();
                if let Some(workdir) = self.current_project().map(|p| p.workdir.clone()) {
                    self.apply_default_workdir_mode(&id, &workdir);
                }
                self.sessions.insert(0, sess);
                self.select_session_inner(id, true, window, cx);
            }
            Err(e) => self.note(NoteKind::Error, format!("{e:#}")),
        }
    }

    fn reload_project_state(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project_id) = self.selected_project_id.clone() else {
            self.quests.clear();
            self.sessions.clear();
            self.selected_session_id = None;
            self.clear_attachments();
            self.selected_prompt_skills.clear();
            self.messages.clear();
            self.session_markdown_views.clear();
            self.live_markdown_view = None;
            self.selected_tool_turn_id = None;
            self.tools_visible_limit = TOOLS_HISTORY_PAGE_SIZE;
            self.clear_turn_anchor();
            self.invalidate_feed();
            self.session_workdirs.clear();
            self.reload_workspace_files(cx);
            return;
        };

        self.quests = self.storage.list_quests(&project_id).unwrap_or_default();
        self.sessions = self.storage.list_sessions(&project_id).unwrap_or_default();
        // Preload all session workdirs for this project so `render()` never
        // needs to hit SQLite per session row.
        self.session_workdirs = self
            .storage
            .list_session_workdirs_for_project(&project_id)
            .unwrap_or_default()
            .into_iter()
            .map(|sw| (sw.session_id.clone(), sw))
            .collect();

        if self.sessions.is_empty() {
            if let Ok(mut sess) = self.storage.create_session(
                &project_id,
                None,
                DEFAULT_SESSION_TITLE,
                &self.config.default_provider_id,
                None,
            ) {
                let default_mode = self.config.default_approval_mode;
                let _ = self
                    .storage
                    .set_session_approval_mode(&sess.id, Some(default_mode));
                sess.approval_mode = Some(default_mode);
                if let Some(workdir) = self.current_project().map(|p| p.workdir.clone()) {
                    self.apply_default_workdir_mode(&sess.id, &workdir);
                }
                self.sessions.push(sess);
            }
        }

        let keep_selected = self
            .selected_session_id
            .as_deref()
            .is_some_and(|id| self.sessions.iter().any(|s| s.id == id));
        if !keep_selected {
            self.selected_session_id = self.sessions.first().map(|s| s.id.clone());
            self.clear_attachments();
            self.selected_prompt_skills.clear();
        }

        self.expanded_dirs.clear();
        self.reload_messages(window, cx);
        self.reload_workspace_files(cx);
        self.scroll_feed_to_end();
    }

    fn reload_messages(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.clear_turn_anchor();
        self.session_markdown_views.clear();
        self.selected_tool_turn_id = None;
        self.tools_visible_limit = TOOLS_HISTORY_PAGE_SIZE;
        if let Some(sid) = self.selected_session_id.as_deref() {
            self.messages = self.storage.list_messages(sid).unwrap_or_default();
            self.invalidate_feed();
        } else {
            self.messages.clear();
            self.invalidate_feed();
        }
        self.sync_live_markdown_for_selected(cx);
    }

    fn current_workspace_root(&self) -> Option<PathBuf> {
        self.selected_session_id
            .as_deref()
            .and_then(|sid| self.effective_workdir_for_session(sid))
            .or_else(|| self.current_project().map(|p| PathBuf::from(&p.workdir)))
    }

    pub(super) fn cached_branch_for_path(&self, path: &Path) -> Option<String> {
        if self.git_info.is_repo
            && self.loaded_workspace_root.as_deref() == Some(path)
            && !self.git_info.branch.is_empty()
            && !self.git_info.branch.starts_with('(')
        {
            return Some(self.git_info.branch.clone());
        }
        None
    }

    fn reload_workspace_files_only(&mut self, cx: &Context<Self>) {
        let Some(root) = self.current_workspace_root() else {
            self.workspace_files_generation = self.workspace_files_generation.wrapping_add(1);
            self.workspace_files_in_flight = false;
            self.workspace_files_pending_refresh = false;
            self.file_entries.clear();
            return;
        };
        if self.workspace_files_in_flight {
            self.workspace_files_pending_refresh = true;
            return;
        }
        let filter = self.file_filter_input.read(cx).value().to_string();
        self.workspace_files_generation = self.workspace_files_generation.wrapping_add(1);
        self.workspace_files_in_flight = true;
        self.workspace_files_pending_refresh = false;
        let generation = self.workspace_files_generation;
        let expanded = self.expanded_dirs.clone();
        let tx = self.ui_tx.clone();
        self.rt.spawn_blocking(move || {
            let entries = scan_directory(&root, &filter, &expanded, 2000);
            let _ = tx.send(UiAsyncMsg::WorkspaceFilesLoaded {
                generation,
                root,
                entries,
            });
        });
    }

    fn reload_git_and_worktrees_async(&mut self, include_worktrees: bool) {
        let Some(root) = self.current_workspace_root() else {
            self.workspace_git_generation = self.workspace_git_generation.wrapping_add(1);
            self.worktrees_refresh_pending = false;
            self.git_info = GitInfo::default();
            self.cached_worktrees.clear();
            return;
        };
        let include_worktrees = include_worktrees || self.worktrees_refresh_pending;
        if include_worktrees {
            self.worktrees_refresh_pending = true;
        }
        self.workspace_git_generation = self.workspace_git_generation.wrapping_add(1);
        let generation = self.workspace_git_generation;
        let project_workdir = if include_worktrees {
            self.current_project().map(|p| PathBuf::from(&p.workdir))
        } else {
            None
        };
        let tx = self.ui_tx.clone();
        self.rt.spawn_blocking(move || {
            let git_info = load_git_info(&root);
            let worktrees =
                project_workdir.map(|pw| same_core::list_worktrees(&pw).unwrap_or_default());
            let _ = tx.send(UiAsyncMsg::WorkspaceGitLoaded {
                generation,
                root,
                git_info,
                worktrees,
            });
        });
    }

    fn reload_workspace_files(&mut self, cx: &Context<Self>) {
        let root_opt = self.current_workspace_root();
        let root_changed = root_opt != self.loaded_workspace_root;
        self.loaded_workspace_root = root_opt.clone();
        if root_changed {
            // Invalidate stale checkout data immediately so clicks or prompt
            // handoffs cannot reference files or branches from the previous checkout.
            self.file_entries.clear();
            self.git_info = GitInfo::default();
            self.cached_worktrees.clear();
        }
        if let Some(root) = root_opt {
            // Clear editor path if it points into a different checkout (e.g.
            // after switching between Shared and an isolated worktree) and has
            // no unsaved edits, so saving cannot write to the wrong checkout.
            if let Some(ref open_path) = self.opened_file_path {
                if !open_path.starts_with(&root) && !self.file_dirty {
                    self.opened_file_path = None;
                    self.opened_file_saved.clear();
                }
            }
            self.reload_workspace_files_only(cx);
            self.reload_git_and_worktrees_async(true);
        } else {
            self.workspace_files_generation = self.workspace_files_generation.wrapping_add(1);
            self.workspace_git_generation = self.workspace_git_generation.wrapping_add(1);
            self.workspace_files_in_flight = false;
            self.workspace_files_pending_refresh = false;
            self.worktrees_refresh_pending = false;
            self.file_entries.clear();
            self.git_info = GitInfo::default();
            self.cached_worktrees.clear();
            self.reconcile_git_tab_visibility();
        }
    }

    fn reconcile_git_tab_visibility(&mut self) {
        if !self.git_info.is_repo {
            if self.right_tab == RightTab::Git {
                self.right_tab = if self.right_split && self.right_bottom_tab == RightTab::Files {
                    RightTab::Tools
                } else {
                    RightTab::Files
                };
            }
            if self.right_bottom_tab == RightTab::Git {
                self.right_bottom_tab = if self.right_tab == RightTab::Tools {
                    RightTab::Logs
                } else {
                    RightTab::Tools
                };
            }
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
            s.title = title.clone();
        }
        if let Some(s) = self.active_session_meta.get_mut(&sid) {
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
        if self
            .live_markdown_view
            .as_ref()
            .is_some_and(|v| v.session_id == sid)
        {
            self.live_markdown_view = None;
        }
        self.agent_manager.release_session(&sid);
        self.active_runs.remove(&sid);
        self.active_session_meta.remove(&sid);
        self.pending_permissions.retain(|p| p.session_id != sid);
        self.pending_inputs.retain(|p| p.request.session_id != sid);
        self.session_ui_states.remove(&sid);
        self.session_workdirs.remove(&sid);
        self.background_alerts.retain(|a| a.session_id != sid);
        let _ = self.storage.delete_session(&sid);
        self.sessions.retain(|x| x.id != sid);
        if self.selected_session_id.as_deref() == Some(sid.as_str()) {
            self.selected_session_id = self.sessions.first().map(|x| x.id.clone());
            self.clear_attachments();
            self.selected_prompt_skills.clear();
            self.pending_provider_switch = None;
            self.pending_worktree_confirm = None;
            if self.selected_session_id.is_none() {
                // Always keep a session to type into.
                self.reload_project_state(window, cx);
            } else {
                self.reload_messages(window, cx);
                self.reload_workspace_files(cx);
            }
            if let Some(new_sid) = self.selected_session_id.clone() {
                if let Some(offset) = self.restore_session_ui_state(&new_sid, window, cx) {
                    self.sync_feed();
                    self.feed.scroll_to(offset);
                } else {
                    self.scroll_feed_to_end();
                }
            }
        }
    }

    fn select_project(&mut self, pid: String, window: &mut Window, cx: &mut Context<Self>) {
        self.select_project_with_session(pid, None, window, cx);
    }

    pub(super) fn select_project_with_session(
        &mut self,
        pid: String,
        target_session_id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(old_sid) = self.selected_session_id.clone() {
            self.save_session_ui_state(&old_sid, cx);
        }
        self.selected_project_id = Some(pid);
        self.selected_quest_filter = None;
        self.selected_session_id = target_session_id;
        self.pending_provider_switch = None;
        self.pending_worktree_confirm = None;
        self.opened_file_path = None;
        self.file_dirty = false;
        self.active_view = MainView::Chat;
        self.confirm_delete = None;
        if !self.file_filter_input.read(cx).value().is_empty() {
            self.suppress_file_filter_reload = true;
            self.file_filter_input
                .update(cx, |i, cx| i.set_value("", window, cx));
            self.suppress_file_filter_reload = false;
        }
        self.reload_project_state(window, cx);
        if let Some(new_sid) = self.selected_session_id.clone() {
            if let Some(offset) = self.restore_session_ui_state(&new_sid, window, cx) {
                self.sync_feed();
                self.feed.scroll_to(offset);
            }
        }
        self.reload_skills_state_fast();
        self.trigger_external_skill_discovery();
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
        let _ = self
            .storage
            .invalidate_legacy_provider_sessions_for_project(&proj.id);
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
        let lang = self.config.language;

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
                let (ok, message) = if !check.available {
                    (false, check.detail)
                } else {
                    match check.auth_status {
                        ProviderAuthStatus::NotLoggedIn => (
                            false,
                            format!(
                                "{} · {}",
                                t(lang, "未ログイン", "Not logged in"),
                                check.detail
                            ),
                        ),
                        ProviderAuthStatus::NoSubscription => (
                            false,
                            format!(
                                "{} · {}",
                                t(lang, "サブスクリプションなし", "No active subscription"),
                                check.detail
                            ),
                        ),
                        ProviderAuthStatus::LoggedIn | ProviderAuthStatus::Unknown => {
                            (true, format!("CLI Ready: {}", check.detail))
                        }
                    }
                };
                let _ = tx.send(UiAsyncMsg::ConnectResult {
                    provider_id: pid,
                    ok,
                    message,
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
        let opening = if same_agent::exec::supports_console_window() {
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

    fn start_in_app_login(
        &mut self,
        provider: ProviderConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some((_, prev_pid, prev)) = self.active_login.take() {
            prev.cancel();
            self.setup_running.remove(&prev_pid);
        }
        self.next_login_attempt_id = self.next_login_attempt_id.wrapping_add(1);
        let attempt_id = self.next_login_attempt_id;
        let preview = self.setup_command_preview(&provider, ProviderSetup::Login);
        self.login_results
            .insert(provider.id.clone(), format!("$ {preview}\n"));
        self.setup_running.insert(provider.id.clone());
        self.login_code_input.update(cx, |i, cx| {
            i.set_value(String::new(), window, cx);
        });

        let pid = provider.id.clone();
        let pid_ev = pid.clone();
        let tx = self.ui_tx.clone();
        let cwd = self.exec_cwd();
        let pref = self.exec_pref();
        let handle =
            self.agent_manager
                .start_in_app_login(provider, pref, cwd, move |ev| match ev {
                    InteractiveLoginEvent::Output(chunk) => {
                        let _ = tx.send(UiAsyncMsg::LoginOutput {
                            attempt_id,
                            provider_id: pid_ev.clone(),
                            chunk,
                        });
                    }
                    InteractiveLoginEvent::Done(result) => {
                        let _ = tx.send(UiAsyncMsg::LoginDone {
                            attempt_id,
                            provider_id: pid_ev.clone(),
                            result,
                        });
                    }
                });
        self.active_login = Some((attempt_id, pid, handle));
    }

    fn submit_in_app_login_code(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let code = self.login_code_input.read(cx).value().trim().to_string();
        if code.is_empty() {
            return;
        }
        if let Some((_, pid, handle)) = &self.active_login {
            let pid = pid.clone();
            if handle.send_line(code) {
                self.login_code_input.update(cx, |i, cx| {
                    i.set_value(String::new(), window, cx);
                });
                let note = t(
                    self.config.language,
                    "\n認証コードを送信しました。確認中…\n",
                    "\nAuthorization code submitted. Verifying…\n",
                );
                self.login_results.entry(pid).or_default().push_str(note);
            } else {
                let note = t(
                    self.config.language,
                    "\n認証コードの送信に失敗しました（プロセスが標準入力を閉じています）。\n",
                    "\nFailed to send authorization code (login process closed stdin).\n",
                );
                self.login_results.entry(pid).or_default().push_str(note);
            }
        }
    }

    fn cancel_in_app_login(&mut self, provider_id: &str) {
        let matches = self
            .active_login
            .as_ref()
            .is_some_and(|(_, pid, _)| pid == provider_id);
        if matches {
            if let Some((_, pid, handle)) = self.active_login.take() {
                handle.cancel();
                self.setup_running.remove(&pid);
            }
        }
    }

    fn check_for_updates(&mut self, manual: bool) {
        if matches!(
            self.update_state,
            UpdateState::Checking | UpdateState::Downloading(_)
        ) {
            return;
        }
        self.update_state = UpdateState::Checking;
        let tx = self.ui_tx.clone();
        self.rt.spawn(async move {
            let result = updater::check_for_updates()
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(UiAsyncMsg::UpdateChecked { result, manual });
        });
    }

    fn install_update(&mut self, release: ReleaseInfo) {
        if matches!(self.update_state, UpdateState::Downloading(_)) {
            return;
        }
        let version = release.version.clone();
        self.update_state = UpdateState::Downloading(release.clone());
        let tx = self.ui_tx.clone();
        self.rt.spawn(async move {
            let result = updater::download_and_install(&release)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(UiAsyncMsg::UpdateInstalled { version, result });
        });
    }

    fn is_logs_tab_visible(&self) -> bool {
        self.show_right_panel
            && (self.right_tab == RightTab::Logs
                || (self.right_split && self.right_bottom_tab == RightTab::Logs))
    }

    /// Returns `(needs_notify, live_markdown_dirty)`.
    fn handle_async_msg(&mut self, msg: UiAsyncMsg, cx: &mut Context<Self>) -> (bool, bool) {
        match msg {
            UiAsyncMsg::WorkspaceFilesLoaded {
                generation,
                root,
                entries,
            } => {
                let mut updated = false;
                if self.workspace_files_generation == generation {
                    self.workspace_files_in_flight = false;
                    if self.loaded_workspace_root.as_ref() == Some(&root) {
                        self.file_entries = entries;
                        updated = true;
                    }
                    if self.workspace_files_pending_refresh {
                        self.workspace_files_pending_refresh = false;
                        self.reload_workspace_files_only(cx);
                    }
                }
                return (updated, false);
            }
            UiAsyncMsg::WorkspaceGitLoaded {
                generation,
                root,
                git_info,
                worktrees,
            } => {
                if self.workspace_git_generation == generation
                    && self.loaded_workspace_root.as_ref() == Some(&root)
                {
                    self.git_info = git_info;
                    if let Some(wts) = worktrees {
                        self.cached_worktrees = wts;
                        self.worktrees_refresh_pending = false;
                    }
                    self.reconcile_git_tab_visibility();
                    return (true, false);
                }
                return (false, false);
            }
            UiAsyncMsg::ExternalSkillsDiscovered { generation, skills } => {
                if self.skills_ui.discovery_generation == generation {
                    self.skills_ui.discovering = false;
                    self.skills_ui.external_skills = skills;
                    self.recompute_skill_resolution();
                } else {
                    return (false, false);
                }
            }
            UiAsyncMsg::SkillDraftGenerated { request_id, result } => {
                if self.skills_ui.ai_request_id != request_id {
                    return (false, false);
                }
                let lang = self.config.language;
                self.skills_ui.ai_generating = false;
                match result {
                    Ok(draft) => {
                        self.skills_ui.ai_error = None;
                        self.skills_ui.pending_generated_draft = Some(draft);
                        self.pending_notes.push((
                            NoteKind::Success,
                            t(
                                lang,
                                "AIがスキルの下書きを生成しました。内容を確認・編集して「作成」を押してください。",
                                "AI generated a skill draft. Review or edit it, then click Create.",
                            )
                            .to_string(),
                        ));
                    }
                    Err(err) => {
                        self.skills_ui.ai_error = Some(err.clone());
                        self.pending_notes.push((
                            NoteKind::Error,
                            match lang {
                                Language::Ja => format!("AI生成に失敗しました: {err}"),
                                Language::En => format!("AI generation failed: {err}"),
                            },
                        ));
                    }
                }
            }
            UiAsyncMsg::SkillsSynced { result, manual } => {
                let lang = self.config.language;
                self.skills_ui.syncing = false;
                self.skills_ui.deployments = self
                    .storage
                    .list_skill_deployments(None)
                    .unwrap_or_default();
                match result {
                    Ok(count) if manual => {
                        self.pending_notes.push((
                            NoteKind::Success,
                            match lang {
                                Language::Ja => {
                                    format!("{count} 件のエージェントへスキルを同期しました")
                                }
                                Language::En => format!("Synced skills for {count} provider(s)"),
                            },
                        ));
                    }
                    Err(err) => {
                        self.pending_notes.push((
                            NoteKind::Error,
                            match lang {
                                Language::Ja => format!("スキル同期エラー: {err}"),
                                Language::En => format!("Skill sync failed: {err}"),
                            },
                        ));
                    }
                    _ => {}
                }
            }
            UiAsyncMsg::UpdateChecked { result, manual } => {
                let lang = self.config.language;
                match result {
                    Ok(UpdateCheckOutcome::Available(info)) => {
                        let ver = info.version.clone();
                        self.update_state = UpdateState::Available(info);
                        self.pending_notes.push((
                            NoteKind::Info,
                            match lang {
                                Language::Ja => format!("新しいバージョン v{ver} が利用可能です"),
                                Language::En => format!("New version v{ver} is available"),
                            },
                        ));
                    }
                    Ok(UpdateCheckOutcome::UpToDate { .. }) => {
                        self.update_state = UpdateState::UpToDate;
                        if manual {
                            self.pending_notes.push((
                                NoteKind::Success,
                                t(
                                    lang,
                                    "最新バージョンを使用しています",
                                    "You are on the latest version",
                                )
                                .to_string(),
                            ));
                        }
                    }
                    Err(err) => {
                        if manual {
                            self.update_state = UpdateState::Error(err.clone());
                            self.pending_notes.push((
                                NoteKind::Error,
                                match lang {
                                    Language::Ja => {
                                        format!("アップデート確認に失敗しました: {err}")
                                    }
                                    Language::En => format!("Update check failed: {err}"),
                                },
                            ));
                        } else {
                            self.update_state = UpdateState::Idle;
                        }
                    }
                }
            }
            UiAsyncMsg::UpdateInstalled { version, result } => {
                let lang = self.config.language;
                match result {
                    Ok(()) => {
                        self.update_state = UpdateState::Installed(version.clone());
                        self.pending_notes.push((
                            NoteKind::Success,
                            match lang {
                                Language::Ja => {
                                    format!("v{version} へのアップデートが完了しました。再起動して適用してください")
                                }
                                Language::En => {
                                    format!("Updated to v{version}. Restart SAME to apply")
                                }
                            },
                        ));
                    }
                    Err(err) => {
                        self.update_state = UpdateState::Error(err.clone());
                        self.pending_notes.push((
                            NoteKind::Error,
                            match lang {
                                Language::Ja => format!("アップデートに失敗しました: {err}"),
                                Language::En => format!("Update failed: {err}"),
                            },
                        ));
                    }
                }
            }
            UiAsyncMsg::ProviderCheck(generation, res) => {
                if generation == self.check_generation {
                    // Once per provider and environment per run, ask a
                    // working agent for its real model list.
                    let key = (res.provider_id.clone(), res.target.clone());
                    if res.available
                        && res.auth_status.is_usable()
                        && !self.models_discovered.contains(&key)
                    {
                        if let Some(p) = self.config.find_provider(&res.provider_id).cloned() {
                            self.start_model_discovery(p, false);
                        }
                    }
                    self.provider_checks.insert(res.provider_id.clone(), res);
                }
            }
            UiAsyncMsg::QuotaFetched {
                provider_id,
                generation,
                context,
                target,
                result,
            } => self.on_quota_fetched(provider_id, generation, context, target, result),
            UiAsyncMsg::StatsFetched {
                provider_id,
                days,
                context,
                target,
                result,
            } => self.on_stats_fetched(provider_id, days, context, target, result),
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
                let pid_for_quota = provider_id.clone();
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
                self.invalidate_and_refresh_quotas(Some(&pid_for_quota));
            }
            UiAsyncMsg::LoginOutput {
                attempt_id,
                provider_id,
                chunk,
            } => {
                if self
                    .active_login
                    .as_ref()
                    .is_some_and(|(id, pid, _)| *id == attempt_id && pid == &provider_id)
                {
                    self.login_results
                        .entry(provider_id)
                        .or_default()
                        .push_str(&chunk);
                }
            }
            UiAsyncMsg::LoginDone {
                attempt_id,
                provider_id,
                result,
            } => {
                let is_current = self
                    .active_login
                    .as_ref()
                    .is_some_and(|(id, pid, _)| *id == attempt_id && pid == &provider_id);
                if !is_current {
                    return (false, false);
                }
                self.active_login = None;
                self.setup_running.remove(&provider_id);
                let pid_for_quota = provider_id.clone();
                let lang = self.config.language;
                match result {
                    Ok(_) => {
                        let entry = self.login_results.entry(provider_id).or_default();
                        if !entry.ends_with('\n') && !entry.is_empty() {
                            entry.push('\n');
                        }
                        entry.push_str(t(
                            lang,
                            "ログインが完了しました。",
                            "Signed in successfully.",
                        ));
                    }
                    Err(e) => {
                        let entry = self.login_results.entry(provider_id).or_default();
                        if !entry.ends_with('\n') && !entry.is_empty() {
                            entry.push('\n');
                        }
                        entry.push_str(&format!("[{e}]"));
                    }
                }
                self.refresh_provider_checks();
                self.invalidate_and_refresh_quotas(Some(&pid_for_quota));
            }
            UiAsyncMsg::Event(ev) => return self.handle_same_event(ev, cx),
        }
        (true, false)
    }

    /// Returns `(needs_notify, live_markdown_dirty)`.
    fn handle_same_event(&mut self, ev: SameEvent, cx: &mut Context<Self>) -> (bool, bool) {
        match ev {
            SameEvent::SessionStarted { session_id, .. } => {
                self.live_drafts.entry(session_id.clone()).or_default();
                if self.is_selected_session(&session_id) {
                    self.mark_feed_tail_dirty();
                }
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
                if self.is_selected_session(&session_id) {
                    self.mark_feed_tail_dirty();
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
                    s.external_session_id = Some(external_session_id.clone());
                }
                if let Ok(Some(sess)) = self.storage.get_session(&session_id) {
                    self.record_provider_session_binding(
                        &session_id,
                        &sess.provider_id,
                        &external_session_id,
                        sess.model.clone(),
                    );
                }
            }
            SameEvent::ProviderSessionBound {
                session_id,
                provider_id,
                external_session_id,
            } => {
                let model = self
                    .sessions
                    .iter()
                    .find(|s| s.id == session_id)
                    .and_then(|s| s.model.clone())
                    .or_else(|| {
                        self.storage
                            .get_session(&session_id)
                            .ok()
                            .flatten()
                            .and_then(|s| s.model)
                    });
                self.record_provider_session_binding(
                    &session_id,
                    &provider_id,
                    &external_session_id,
                    model,
                );
            }
            SameEvent::ContextHandoffApplied {
                session_id,
                from_provider_id,
                to_provider_id,
                mode,
                is_delta,
            } => {
                let stamp = Local::now().format("%H:%M:%S");
                self.logs.push(format!(
                    "{stamp} [handoff] session={} from={:?} to={} mode={} delta={}",
                    session_id,
                    from_provider_id,
                    to_provider_id,
                    mode.as_str(),
                    is_delta
                ));
            }
            SameEvent::NativeSessionFallback {
                session_id,
                provider_id,
                reason,
            } => {
                let (exec_key, workdir) = self
                    .active_runs
                    .get(&session_id)
                    .map(|r| (r.exec_mode_key.clone(), r.workdir.clone()))
                    .unwrap_or_else(|| {
                        (
                            self.exec_mode_key().to_string(),
                            self.effective_workdir_for_session(&session_id)
                                .map(|p| simplify_path(&p.to_string_lossy()))
                                .unwrap_or_default(),
                        )
                    });
                if let Ok(Some(old_ps)) = self.storage.get_active_provider_session(
                    &session_id,
                    &provider_id,
                    &exec_key,
                    &workdir,
                ) {
                    let _ = self.storage.invalidate_provider_session(&old_ps.id);
                }
                let lang = self.config.language;
                let pname = self.provider_name(&provider_id);
                self.note(
                    NoteKind::Info,
                    match lang {
                        Language::Ja => format!(
                            "{pname} の既存セッションを再開できなかったため、コンテキストを引き継いで新規セッションで継続しました"
                        ),
                        Language::En => format!(
                            "Could not resume {pname} native session; continued in a new session with context handoff"
                        ),
                    },
                );
                let stamp = Local::now().format("%H:%M:%S");
                self.logs
                    .push(format!("{stamp} [{provider_id}] fallback: {reason}"));
            }
            SameEvent::MessageDelta { session_id, text } => {
                if let Some(d) = self.live_drafts.get_mut(&session_id) {
                    d.push_content(&text);
                } else {
                    return (false, false);
                }
                if self.is_selected_session(&session_id) {
                    self.mark_feed_tail_dirty();
                    return (true, true);
                }
                return (false, false);
            }
            SameEvent::ThinkingDelta { session_id, text } => {
                if let Some(d) = self.live_drafts.get_mut(&session_id) {
                    d.thinking.push_str(&text);
                    if self.is_selected_session(&session_id) {
                        self.mark_feed_tail_dirty();
                        return (true, false);
                    }
                }
                return (false, false);
            }
            SameEvent::ToolCallStarted {
                session_id,
                tool_call,
            } => {
                let Some(draft) = self.live_drafts.get_mut(&session_id) else {
                    return (false, false);
                };
                if let Some(existing) = draft.tool_calls.iter_mut().find(|t| t.id == tool_call.id) {
                    *existing = tool_call;
                } else {
                    draft.tool_calls.push(tool_call);
                }
                if self.is_selected_session(&session_id) {
                    self.mark_feed_tail_dirty();
                    return (true, false);
                }
                return (false, false);
            }
            SameEvent::ToolCallUpdated {
                session_id,
                tool_call,
            } => {
                let Some(draft) = self.live_drafts.get_mut(&session_id) else {
                    return (false, false);
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
                if self.is_selected_session(&session_id) {
                    self.mark_feed_tail_dirty();
                    return (true, false);
                }
                return (false, false);
            }
            SameEvent::ApprovalRequested { request } => {
                // A request that arrives after the run was stopped can't be
                // acted on; decline it so the agent is not left waiting.
                if !self.live_drafts.contains_key(&request.session_id) {
                    let mgr = self.agent_manager.clone();
                    let rid = request.request_id.clone();
                    self.rt
                        .spawn(async move { mgr.respond_permission(&rid, None).await });
                    return (false, false);
                }
                if !self.is_selected_session(&request.session_id) {
                    let title = self.session_title_by_id(&request.session_id);
                    let detail = t(self.config.language, "承認待ち", "Approval needed").to_string();
                    self.note(NoteKind::Warning, format!("{detail}: {title}"));
                    self.push_background_alert(&request.session_id, NoteKind::Warning, detail);
                } else {
                    self.focus_approval = true;
                    self.mark_feed_tail_dirty();
                }
                self.notify_waiting(
                    &request.session_id,
                    t(self.config.language, "承認待ち", "Approval needed"),
                );
                self.pending_permissions.push(request);
            }
            SameEvent::UserInputRequested { request } => {
                if !self.live_drafts.contains_key(&request.session_id) {
                    let mgr = self.agent_manager.clone();
                    let rid = request.request_id.clone();
                    self.rt
                        .spawn(async move { mgr.respond_user_input(&rid, None).await });
                    return (false, false);
                }
                let lang = self.config.language;
                if self.is_selected_session(&request.session_id) {
                    self.focus_approval = true;
                    self.mark_feed_tail_dirty();
                } else {
                    let title = self.session_title_by_id(&request.session_id);
                    let detail = t(lang, "質問があります", "Question for you").to_string();
                    self.note(NoteKind::Warning, format!("{detail}: {title}"));
                    self.push_background_alert(&request.session_id, NoteKind::Warning, detail);
                }
                self.notify_waiting(
                    &request.session_id,
                    t(lang, "質問があります", "Question for you"),
                );
                self.pending_inputs.push(PendingInput::new(request));
            }
            SameEvent::UserInputWithdrawn {
                session_id,
                request_id,
            } => {
                self.pending_inputs
                    .retain(|p| p.request.request_id != request_id);
                if self.is_selected_session(&session_id) {
                    self.mark_feed_tail_dirty();
                }
            }
            SameEvent::PlanProposed { session_id, plan } => {
                if let Some(draft) = self.live_drafts.get_mut(&session_id) {
                    draft.plan = Some(plan);
                    if self.is_selected_session(&session_id) {
                        self.mark_feed_tail_dirty();
                    }
                }
            }
            SameEvent::ApprovalWithdrawn {
                session_id,
                request_id,
                remaining,
            } => {
                self.pending_permissions
                    .retain(|p| p.request_id != request_id);
                if self.is_selected_session(&session_id) {
                    self.mark_feed_tail_dirty();
                }
                if remaining == 0 && self.live_drafts.contains_key(&session_id) {
                    if let Some(s) = self.sessions.iter_mut().find(|s| s.id == session_id) {
                        if s.status == SessionStatus::WaitingForApproval {
                            s.status = SessionStatus::Running;
                            let _ = self.storage.update_session_meta(
                                &session_id,
                                None,
                                None,
                                None,
                                None,
                                Some(SessionStatus::Running),
                            );
                        }
                    }
                }
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
                self.finalize_live_draft(&session_id, SessionStatus::Completed, None, cx);
                self.refresh_quota_after_turn(&session_id);
            }
            SameEvent::SessionCancelled { session_id } => {
                self.finalize_live_draft(&session_id, SessionStatus::Cancelled, None, cx);
            }
            SameEvent::SessionFailed { session_id, error } => {
                let same_exec_context = self
                    .live_drafts
                    .get(&session_id)
                    .is_some_and(|d| d.exec_context == self.exec_label());
                if same_exec_context && same_agent::is_claude_subscription_error(&error) {
                    if let Some(pid) = self
                        .sessions
                        .iter()
                        .find(|s| s.id == session_id)
                        .map(|s| s.provider_id.clone())
                    {
                        let is_claude = self
                            .config
                            .find_provider(&pid)
                            .is_some_and(|p| p.kind == ProviderKind::ClaudeCli);
                        if is_claude {
                            if let Some(chk) = self.provider_checks.get_mut(&pid) {
                                chk.auth_status = ProviderAuthStatus::NoSubscription;
                            }
                        }
                    }
                }
                self.finalize_live_draft(&session_id, SessionStatus::Failed, Some(error), cx);
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
                return (self.is_logs_tab_visible(), false);
            }
            SameEvent::ProviderCapabilitiesUpdated { .. } => {
                return (false, false);
            }
            SameEvent::SkillCreated { .. }
            | SameEvent::SkillUpdated { .. }
            | SameEvent::SkillDeleted { .. }
            | SameEvent::SkillImported { .. }
            | SameEvent::SkillEnabled { .. }
            | SameEvent::SkillDisabled { .. }
            | SameEvent::SkillAssignmentChanged { .. } => {
                self.reload_skills_state();
            }
            SameEvent::SkillSyncStarted { .. } => {
                self.skills_ui.syncing = true;
            }
            SameEvent::SkillSyncCompleted { .. } => {
                self.skills_ui.syncing = false;
                self.skills_ui.deployments = self
                    .storage
                    .list_skill_deployments(None)
                    .unwrap_or_default();
            }
            SameEvent::SkillSyncFailed {
                provider_id, error, ..
            } => {
                self.skills_ui.syncing = false;
                self.logs
                    .push(format!("[skills] sync failed for {provider_id}: {error}"));
            }
            SameEvent::SkillConflictDetected { skill_name, detail } => {
                self.logs
                    .push(format!("[skills] conflict on '{skill_name}': {detail}"));
            }
        }
        (true, false)
    }

    fn finalize_live_draft(
        &mut self,
        session_id: &str,
        status: SessionStatus,
        error: Option<String>,
        cx: &mut Context<Self>,
    ) {
        // A run can report both failure and a final status; only the first
        // one owns the draft.
        let Some(draft) = self.live_drafts.remove(session_id) else {
            return;
        };
        let live_md = if self
            .live_markdown_view
            .as_ref()
            .is_some_and(|v| v.session_id == session_id)
        {
            self.live_markdown_view.take()
        } else {
            None
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
        if let Some(plan) = draft.plan.as_deref().filter(|p| !p.trim().is_empty()) {
            content.push_str(PLAN_MARKER);
            content.push_str(plan);
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
        let active_run = self.active_runs.remove(session_id);
        let mut last_msg_id: Option<String> = None;
        let has_message =
            !content.is_empty() || !draft.thinking.is_empty() || !tool_calls.is_empty();
        if has_message {
            let msg = Message {
                id: draft.id.clone(),
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
            last_msg_id = Some(msg.id.clone());
            let _ = self.storage.append_message(&msg);
            if self.is_selected_session(session_id) {
                if let Some(anchor) = self.feed_turn_anchor.borrow_mut().as_mut() {
                    let live_h = self.feed_live_content_height.get();
                    if anchor.session_id == session_id && live_h > px(0.0) {
                        anchor.measured_assistant_height = Some((live_h - px(8.0)).max(px(0.0)));
                    }
                }
                if let Some(live_md) = live_md {
                    let final_body = message_markdown_body(&msg.content);
                    self.handoff_finalized_markdown(session_id, &msg.id, final_body, live_md, cx);
                }
                self.messages.push(msg);
                self.sync_feed_after_finalize();
            }
        } else {
            self.expanded_thinking.remove(&draft.id);
            let prefix = format!("{}-", draft.id);
            self.expanded_tools.retain(|k| !k.starts_with(&prefix));
            if self.is_selected_session(session_id) {
                self.clear_turn_anchor();
                self.mark_feed_tail_dirty();
            }
        }
        if let Some(run_info) = active_run {
            let run_status = match status {
                SessionStatus::Completed => AgentRunStatus::Completed,
                SessionStatus::Cancelled => AgentRunStatus::Cancelled,
                _ => AgentRunStatus::Failed,
            };
            let _ = self
                .storage
                .finish_agent_run(&run_info.run_id, run_status, error.as_deref());
            // Only advance the provider session sync cursor on successful completion.
            // If handoff/turn failed or was cancelled, keep the previous cursor so
            // unsynced context is never marked as synced.
            if status == SessionStatus::Completed {
                let sync_msg_id = last_msg_id.or_else(|| {
                    self.storage
                        .list_messages(session_id)
                        .ok()
                        .and_then(|ms| ms.last().map(|m| m.id.clone()))
                });
                if let (Some(ps_id), Some(mid)) = (run_info.provider_session_id, sync_msg_id) {
                    let model = sess.as_ref().and_then(|s| s.model.as_deref());
                    let _ = self
                        .storage
                        .mark_provider_session_synced(&ps_id, &mid, model);
                }
                // Clear persisted Fresh handoff flag only after a turn actually
                // succeeded so failed attempts never retry with prior history.
                if sess.as_ref().and_then(|s| s.handoff_mode) == Some(HandoffMode::Fresh) {
                    let _ = self.storage.set_session_handoff_mode(session_id, None);
                    if let Some(s) = self.sessions.iter_mut().find(|x| x.id == session_id) {
                        s.handoff_mode = None;
                    }
                    if let Some(st) = self.session_ui_states.get_mut(session_id) {
                        st.pending_fresh_handoff = false;
                        if st.handoff_mode == Some(HandoffMode::Fresh) {
                            st.handoff_mode = None;
                        }
                    }
                }
            }
        }
        if self.is_selected_session(session_id) {
            self.feed_live_content_height.set(px(0.0));
        }
        let _ = self
            .storage
            .update_session_meta(session_id, None, None, None, None, Some(status));
        if let Some(s) = self.sessions.iter_mut().find(|s| s.id == session_id) {
            s.status = status;
        }
        self.pending_permissions
            .retain(|p| p.session_id != session_id);
        self.pending_inputs
            .retain(|p| p.request.session_id != session_id);

        let title = sess
            .as_ref()
            .map(|s| self.session_title(s))
            .unwrap_or_default();
        let visible = self.selected_session_id.as_deref() == Some(session_id);
        match (&error, status) {
            (Some(err), _) => self.os_notify(
                format!("SAME — {}", t(lang, "エラー", "Error")),
                format!("{title}: {}", err.lines().next().unwrap_or("")),
            ),
            (None, SessionStatus::Completed) => self.os_notify(
                format!("SAME — {}", t(lang, "完了", "Finished")),
                title.clone(),
            ),
            _ => {}
        }
        if let (Some(err), false) = (error, visible) {
            let first_line: String = err.lines().next().unwrap_or("").chars().take(160).collect();
            self.session_ui_states
                .entry(session_id.to_string())
                .or_default()
                .has_unread_error = true;
            self.note(
                NoteKind::Error,
                format!(
                    "{}「{title}」: {first_line}",
                    t(lang, "エラー", "Error in ")
                ),
            );
            self.push_background_alert(
                session_id,
                NoteKind::Error,
                format!("{}: {first_line}", t(lang, "エラー", "Error")),
            );
        } else if status == SessionStatus::Completed
            && self.selected_session_id.as_deref() != Some(session_id)
        {
            self.session_ui_states
                .entry(session_id.to_string())
                .or_default()
                .has_unread_completion = true;
            self.note(
                NoteKind::Success,
                format!("{}: {title}", t(lang, "完了", "Finished")),
            );
            self.push_background_alert(
                session_id,
                NoteKind::Success,
                t(lang, "完了", "Finished").to_string(),
            );
        }

        self.active_session_meta.remove(session_id);
        if self.current_workspace_root().is_some() {
            self.reload_workspace_files_only(cx);
            self.reload_git_and_worktrees_async(false);
            // The agent may have edited the file that is open in the panel.
            self.reload_open_file_pending = true;
        }
    }

    fn record_provider_session_binding(
        &mut self,
        session_id: &str,
        provider_id: &str,
        external_session_id: &str,
        model: Option<String>,
    ) {
        let (exec_key, workdir) = self
            .active_runs
            .get(session_id)
            .map(|r| (r.exec_mode_key.clone(), r.workdir.clone()))
            .unwrap_or_else(|| {
                (
                    self.exec_mode_key().to_string(),
                    self.effective_workdir_for_session(session_id)
                        .map(|p| simplify_path(&p.to_string_lossy()))
                        .unwrap_or_default(),
                )
            });
        let now = Utc::now();
        let existing = self
            .storage
            .get_active_provider_session(session_id, provider_id, &exec_key, &workdir)
            .ok()
            .flatten();
        let ps = match existing {
            Some(mut prev) if prev.external_session_id == external_session_id => {
                prev.model = model.or(prev.model);
                if prev.exec_mode.is_empty() {
                    prev.exec_mode = exec_key;
                }
                prev.last_used_at = now;
                prev
            }
            Some(prev) => {
                let _ = self.storage.invalidate_provider_session(&prev.id);
                ProviderSession {
                    id: Uuid::new_v4().to_string(),
                    session_id: session_id.to_string(),
                    provider_id: provider_id.to_string(),
                    external_session_id: external_session_id.to_string(),
                    model,
                    exec_mode: exec_key,
                    workdir,
                    last_synced_message_id: None,
                    status: ProviderSessionStatus::Active,
                    created_at: now,
                    last_used_at: now,
                }
            }
            None => ProviderSession {
                id: Uuid::new_v4().to_string(),
                session_id: session_id.to_string(),
                provider_id: provider_id.to_string(),
                external_session_id: external_session_id.to_string(),
                model,
                exec_mode: exec_key,
                workdir,
                last_synced_message_id: None,
                status: ProviderSessionStatus::Active,
                created_at: now,
                last_used_at: now,
            },
        };
        let ps_id = ps.id.clone();
        let _ = self.storage.upsert_provider_session(&ps);
        if let Some(run_info) = self.active_runs.get_mut(session_id) {
            run_info.provider_session_id = Some(ps_id.clone());
            let _ = self
                .storage
                .set_agent_run_provider_session(&run_info.run_id, &ps_id);
        }
    }

    fn send_current_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let lang = self.config.language;
        let text = self.prompt_input.read(cx).value().trim().to_string();
        if text.is_empty() && self.attachments.is_empty() && self.selected_prompt_skills.is_empty()
        {
            return;
        }
        // Local commands never reach the agent (and work while it runs).
        if !text.is_empty() && self.try_model_command(&text, window, cx) {
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
        // Preflight duplicate-run and concurrency limit before mutating the composer or DB.
        if let Err(err) = self.agent_manager.can_start_prompt(&session.id) {
            self.note(NoteKind::Warning, format!("{err:#}"));
            return;
        }

        self.prompt_input.update(cx, |ta, cx| {
            ta.set_value("", window, cx);
        });
        self.prompt_empty = true;
        self.prompt_is_command = false;
        self.prompt_is_skill_mention = false;
        let attachments = std::mem::take(&mut self.attachments);
        let picked_skills = std::mem::take(&mut self.selected_prompt_skills);
        self.pasted_attachments.clear();
        let attachment_names: Vec<String> =
            attachments.iter().map(|p| attachment_name(p)).collect();

        // Combine skills picked from the `+` menu with any `$skill-name` mentions in text.
        let mut explicit_skills = picked_skills.clone();
        let available_skills = self.available_prompt_skills();
        for mentioned_name in skills::extract_dollar_skill_names(&text) {
            if let Some(found) = available_skills
                .iter()
                .find(|s| s.name.eq_ignore_ascii_case(&mentioned_name))
            {
                if !explicit_skills.iter().any(|s| s.name == found.name) {
                    explicit_skills.push(found.clone());
                }
            }
        }
        let picked_skill_names: Vec<String> =
            picked_skills.iter().map(|s| s.name.clone()).collect();

        if session.title == DEFAULT_SESSION_TITLE {
            let first_line = text
                .lines()
                .next()
                .map(str::to_string)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| {
                    if !picked_skill_names.is_empty() {
                        picked_skill_names
                            .iter()
                            .map(|n| format!("${n}"))
                            .collect::<Vec<_>>()
                            .join(" ")
                    } else {
                        attachment_names.join(", ")
                    }
                });
            let first_line = first_line.as_str();
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
            // Attachments are noted in the stored text; the agent gets the
            // files themselves.
            content: {
                let mut badges: Vec<String> = picked_skill_names
                    .iter()
                    .map(|n| format!("✦ `${n}`"))
                    .collect();
                badges.extend(attachment_names.iter().map(|n| format!("📎 `{n}`")));
                if badges.is_empty() {
                    text.clone()
                } else {
                    let list = badges.join("  ");
                    if text.is_empty() {
                        list
                    } else {
                        format!("{text}\n\n{list}")
                    }
                }
            },
            thinking: None,
            tool_calls: vec![],
            provider_id: Some(provider.id.clone()),
            model: model.clone(),
            created_at: Utc::now(),
        };
        let should_anchor = self.should_anchor_new_turn();
        let viewport = self.feed.viewport_bounds();
        // `self.messages` already holds the selected session's messages in memory;
        // avoid a synchronous SQLite full-history read + JSON parse on every send.
        let prior_messages = self.messages.clone();
        let user_msg_id = user_msg.id.clone();
        let user_msg_content = user_msg.content.clone();
        let _ = self.storage.append_message(&user_msg);
        self.messages.push(user_msg);

        // Move the session to the top of the list, like most chat apps.
        if let Some(pos) = self.sessions.iter().position(|s| s.id == session.id) {
            let s = self.sessions.remove(pos);
            self.sessions.insert(0, s);
        }

        let cwd = self
            .effective_workdir_for_session(&session.id)
            .unwrap_or_else(|| PathBuf::from(&project.workdir));
        let workdir_mode = self.session_workdir_mode(&session.id);
        let read_only = workdir_mode == WorkdirMode::ReadOnly;
        let norm_workdir = simplify_path(&cwd.to_string_lossy());
        let exec_key = self.exec_mode_key();
        let handoff_mode = self.session_handoff_mode(&session.id);
        let ui_fresh = self
            .session_ui_states
            .get(&session.id)
            .is_some_and(|st| st.pending_fresh_handoff);
        let persisted_fresh = session.handoff_mode == Some(HandoffMode::Fresh);
        // Keep Fresh active until a turn succeeds (cleared in finalize_live_draft)
        // so retries after a failed start never leak pre-Fresh history.
        let consume_fresh = ui_fresh || persisted_fresh;

        // Resolve reusable ProviderSession for `(session, provider, exec_mode, workdir)`.
        // Only suppress reuse on the initial turn of an explicit Fresh handoff;
        // subsequent turns on the newly created native session continue normally.
        let active_ps = if consume_fresh {
            None
        } else {
            self.storage
                .get_active_provider_session(&session.id, &provider.id, exec_key, &norm_workdir)
                .ok()
                .flatten()
        };
        let is_default_project_workdir =
            workdir_mode == WorkdirMode::Shared && norm_workdir == simplify_path(&project.workdir);
        let has_any_provider_sessions = !self
            .storage
            .list_provider_sessions(&session.id)
            .unwrap_or_default()
            .is_empty();
        let external_session_id = active_ps
            .as_ref()
            .map(|ps| ps.external_session_id.clone())
            .or_else(|| {
                // Only fall back to legacy unscoped `session.external_session_id`
                // when still on the default project workdir and no scoped
                // `provider_sessions` exist yet.
                if !consume_fresh && is_default_project_workdir && !has_any_provider_sessions {
                    session.external_session_id.clone()
                } else {
                    None
                }
            });

        let mut base_prompt = text;
        if external_session_id.is_none() && prior_messages.is_empty() {
            if let Some(qid) = session.quest_id.as_deref() {
                if let Some(quest) = self.quests.iter().find(|q| q.id == qid) {
                    if !quest.goal.trim().is_empty() {
                        base_prompt = format!(
                            "[Quest: {}]\nGoal: {}\n\n{}",
                            quest.title, quest.goal, base_prompt
                        );
                    }
                }
            }
        }

        // Build Context Handoff (delta sync on resume, or Smart/Full handoff on new provider session).
        let mut full_prompt = base_prompt.clone();
        let mut fallback_handoff_prompt: Option<String> = None;

        // Respect any persisted Fresh history cutoff so pre-Fresh messages are
        // never disclosed on retries or later native-session fallbacks.
        let handoff_messages: &[Message] = match session.handoff_cutoff_message_id.as_deref() {
            Some(cutoff_id) => match prior_messages.iter().position(|m| m.id == cutoff_id) {
                Some(pos) => &prior_messages[pos + 1..],
                None => &prior_messages,
            },
            None => &prior_messages,
        };

        if !handoff_messages.is_empty() && !consume_fresh {
            let env_snapshot = ContextEnvironment {
                workdir: norm_workdir.clone(),
                branch: same_core::current_branch(&cwd),
                workdir_mode,
            };
            let existing_cp = self
                .storage
                .get_latest_context_checkpoint(&session.id)
                .ok()
                .flatten();
            let from_provider = prior_messages
                .iter()
                .rev()
                .find_map(|m| m.provider_id.as_deref());
            if external_session_id.is_some() {
                // Reusing an existing native session: if we have a scoped
                // ProviderSession, send only the unsynced delta since its cursor.
                if let Some(ref ps) = active_ps {
                    let delta_payload = ContextManager::build_handoff_payload(&HandoffRequest {
                        session_id: &session.id,
                        mode: handoff_mode,
                        messages: handoff_messages,
                        last_synced_message_id: ps.last_synced_message_id.as_deref(),
                        existing_checkpoint: existing_cp.as_ref(),
                        environment: env_snapshot.clone(),
                        max_budget_tokens: None,
                        from_provider_id: from_provider,
                        to_provider_id: &provider.id,
                    });
                    if let Some(prefix) = delta_payload.rendered_prompt {
                        full_prompt = format!("{prefix}{base_prompt}");
                    }
                }
                // Always prepare a fallback handoff prompt when resuming any
                // external ID (including legacy sessions) in case native resume fails.
                let fb_payload = ContextManager::build_handoff_payload(&HandoffRequest {
                    session_id: &session.id,
                    mode: handoff_mode,
                    messages: handoff_messages,
                    last_synced_message_id: None,
                    existing_checkpoint: existing_cp.as_ref(),
                    environment: env_snapshot,
                    max_budget_tokens: None,
                    from_provider_id: from_provider,
                    to_provider_id: &provider.id,
                });
                if let Some(fb_prefix) = fb_payload.rendered_prompt {
                    fallback_handoff_prompt = Some(format!("{fb_prefix}{base_prompt}"));
                }
            } else {
                let payload = ContextManager::build_handoff_payload(&HandoffRequest {
                    session_id: &session.id,
                    mode: handoff_mode,
                    messages: handoff_messages,
                    last_synced_message_id: None,
                    existing_checkpoint: existing_cp.as_ref(),
                    environment: env_snapshot,
                    max_budget_tokens: None,
                    from_provider_id: from_provider,
                    to_provider_id: &provider.id,
                });
                if let Some(cp) = payload.new_checkpoint {
                    let _ = self.storage.insert_context_checkpoint(&cp);
                }
                if let Some(prefix) = payload.rendered_prompt {
                    full_prompt = format!("{prefix}{base_prompt}");
                }
            }
        }

        let mode = if read_only {
            ApprovalMode::Plan
        } else {
            self.session_mode(&session)
        };
        let exec = self.exec_pref();
        let explicit_skill_pairs: Vec<(String, PathBuf)> = explicit_skills
            .into_iter()
            .map(|s| (s.name, s.skill_md_path))
            .collect();

        let run_id = Uuid::new_v4().to_string();
        let run_record = AgentRun {
            id: run_id.clone(),
            session_id: session.id.clone(),
            provider_session_id: active_ps.as_ref().map(|p| p.id.clone()),
            provider_id: provider.id.clone(),
            model: model.clone(),
            status: AgentRunStatus::Running,
            started_at: Utc::now(),
            finished_at: None,
            error: None,
        };
        let _ = self.storage.insert_agent_run(&run_record);
        self.active_runs.insert(
            session.id.clone(),
            parallel::ActiveRunInfo {
                run_id: run_id.clone(),
                provider_session_id: active_ps.as_ref().map(|p| p.id.clone()),
                exec_mode_key: exec_key.to_string(),
                workdir: norm_workdir.clone(),
            },
        );

        let latest_session = self
            .sessions
            .iter()
            .find(|s| s.id == session.id)
            .cloned()
            .unwrap_or_else(|| session.clone());
        self.active_session_meta
            .insert(session.id.clone(), latest_session);
        let mut draft = LiveDraft::new();
        draft.exec_context = self.exec_label();
        self.live_drafts.insert(session.id.clone(), draft);
        if self
            .live_markdown_view
            .as_ref()
            .is_some_and(|v| v.session_id == session.id)
        {
            self.live_markdown_view = None;
        }
        self.feed_live_content_height.set(px(0.0));
        if should_anchor {
            *self.feed_turn_anchor.borrow_mut() = Some(FeedTurnAnchor {
                session_id: session.id.clone(),
                user_msg_id,
                estimated_user_height: estimate_user_row_height(
                    &user_msg_content,
                    viewport.size.height,
                ),
                measured_user_height: None,
                measured_assistant_height: None,
            });
        } else {
            self.clear_turn_anchor();
        }
        self.mark_feed_tail_dirty();
        self.sync_feed();
        self.scroll_feed_to_end();

        let start_res = self.agent_manager.try_start_prompt(PromptRequest {
            session_id: session.id.clone(),
            external_session_id,
            provider,
            model,
            effort: session.effort.clone().filter(|e| !e.trim().is_empty()),
            cwd,
            prompt: full_prompt,
            attachments,
            exec,
            mode,
            read_only,
            fallback_handoff_prompt,
            skill_delivery: None,
            skill_sync: Some(same_agent::PromptSkillSync {
                synchronizer: self.skills_ui.synchronizer.clone(),
                project_id: self.selected_project_id.clone(),
                resolution: self.skills_ui.resolution.clone(),
                explicit_skills: explicit_skill_pairs,
            }),
        });
        if let Err(err) = start_res {
            self.live_drafts.remove(&session.id);
            self.active_runs.remove(&session.id);
            self.active_session_meta.remove(&session.id);
            let err_str = format!("{err:#}");
            let _ = self
                .storage
                .finish_agent_run(&run_id, AgentRunStatus::Failed, Some(&err_str));
            self.note(NoteKind::Warning, err_str);
            self.mark_feed_tail_dirty();
        }
    }

    /// The current session's earlier prompts, newest first, without the
    /// attachment note added when they were stored.
    fn prompt_history(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for m in self.messages.iter().rev() {
            if m.role != MessageRole::User {
                continue;
            }
            let text = strip_attachment_note(&m.content).trim().to_string();
            if text.is_empty() || out.last() == Some(&text) {
                continue;
            }
            out.push(text);
        }
        out
    }

    /// Up / Down in the prompt: walks through earlier prompts, like a shell.
    /// Starts only from an empty prompt, and only moves on while the input
    /// still shows the recalled text with the cursor on its first (Up) or
    /// last (Down) line, so ordinary editing of multi-line text keeps
    /// working. Returns whether the key was used.
    fn recall_prompt_history(
        &mut self,
        older: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let (value, line) = {
            let state = self.prompt_input.read(cx);
            (
                state.value().to_string(),
                state.cursor_position().line as usize,
            )
        };
        let browsing = self
            .history_recall
            .as_ref()
            .filter(|(_, text)| *text == value)
            .map(|(i, _)| *i);
        if browsing.is_none() {
            self.history_recall = None;
        }
        let last_line = value.split('\n').count().saturating_sub(1);
        let next = if older {
            if line != 0 || !(value.is_empty() || browsing.is_some()) {
                return false;
            }
            browsing.map_or(0, |i| i + 1)
        } else {
            match browsing {
                Some(i) if line == last_line => {
                    if i == 0 {
                        // Past the newest: back to an empty prompt.
                        self.history_recall = None;
                        self.set_prompt_text(String::new(), window, cx);
                        return true;
                    }
                    i - 1
                }
                _ => return false,
            }
        };
        let Some(text) = self.prompt_history().into_iter().nth(next) else {
            return false;
        };
        self.history_recall = Some((next, text.clone()));
        self.set_prompt_text(text, window, cx);
        true
    }

    /// Replaces the prompt text, keeping the derived flags in sync.
    fn set_prompt_text(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.prompt_empty = text.trim().is_empty();
        self.prompt_is_command = text.starts_with('/') && !text.contains('\n');
        self.prompt_is_skill_mention = skills::detect_dollar_skill_token(&text).is_some();
        self.prompt_input.update(cx, |ta, cx| {
            ta.set_value(text, window, cx);
        });
        cx.notify();
    }

    /// Opens the file picker and adds the chosen files to the next prompt.
    fn pick_attachments(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let lang = self.config.language;
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some(t(lang, "添付", "Attach").into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = rx.await;
            let _ = this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(Ok(Some(paths))) => this.add_attachments(paths),
                    Ok(Ok(None)) => {}
                    _ => this.note(
                        NoteKind::Warning,
                        t(
                            lang,
                            "ファイル選択ダイアログを開けませんでした。ファイルを入力欄にドラッグ＆ドロップしてください",
                            "Could not open the file picker. Drag and drop files onto the prompt instead",
                        ),
                    ),
                }
                this.prompt_input.update(cx, |ta, cx| ta.focus(window, cx));
                cx.notify();
            });
        })
        .detach();
    }

    /// Adds regular files to the next prompt, skipping directories and ones
    /// already attached.
    fn add_attachments(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        for p in paths {
            if p.is_file() && !self.attachments.contains(&p) {
                self.attachments.push(p);
            }
        }
    }

    /// Drops all pending attachments and deletes any unsent pasted-image
    /// copies created by SAME.
    fn clear_attachments(&mut self) {
        for path in std::mem::take(&mut self.attachments) {
            self.cleanup_pasted_attachment(&path);
        }
    }

    /// Removes a pasted image file created for the current draft. User-selected
    /// files and previously sent files are never touched.
    fn cleanup_pasted_attachment(&mut self, path: &Path) {
        if self.pasted_attachments.remove(path) {
            let _ = std::fs::remove_file(path);
        }
    }

    /// Handles a paste into the composer: images (screenshots, copied image
    /// data) and files copied in a file manager become attachments instead of
    /// text. A paste that also carries unrelated text still inserts that text.
    /// Returns whether the paste was fully consumed.
    fn handle_paste(&mut self, item: &ClipboardItem) -> bool {
        let mut paths = Vec::new();
        let mut copied_files = Vec::new();
        let mut texts = Vec::new();
        let mut failed_image = false;
        for entry in &item.entries {
            match entry {
                ClipboardEntry::Image(image) => match self.save_pasted_image(image) {
                    Some(path) => {
                        self.pasted_attachments.insert(path.clone());
                        paths.push(path);
                    }
                    None => failed_image = true,
                },
                ClipboardEntry::ExternalPaths(external) => {
                    for p in external.paths() {
                        if p.is_file() {
                            copied_files.push(p.clone());
                            paths.push(p.clone());
                        }
                    }
                }
                ClipboardEntry::String(text) if !text.text.is_empty() => {
                    texts.push(text.text.as_str());
                }
                ClipboardEntry::String(_) => {}
            }
        }
        if failed_image {
            let lang = self.config.language;
            self.note(
                NoteKind::Warning,
                t(
                    lang,
                    "貼り付けた画像を保存できませんでした",
                    "Could not save the pasted image",
                ),
            );
        }
        if paths.is_empty() {
            return false;
        }
        self.add_attachments(paths);
        // Copied files also carry their names/paths as text (e.g. Finder);
        // suppress only that companion text, keeping unrelated clipboard text.
        if texts.is_empty() {
            return true;
        }
        !copied_files.is_empty()
            && texts.iter().all(|text| {
                text.lines().all(|line| {
                    let trimmed = line.trim();
                    trimmed.is_empty()
                        || copied_files.iter().any(|p| {
                            p.to_str() == Some(trimmed)
                                || p.file_name().and_then(|n| n.to_str()) == Some(trimmed)
                        })
                })
            })
    }

    /// Writes a clipboard image into the app's data directory so the agent
    /// can read it as a normal file attachment.
    fn save_pasted_image(&self, image: &gpui::Image) -> Option<PathBuf> {
        let ext = image.format.extension();
        if !same_agent::adapters::is_image_path(&format!("image.{ext}")) {
            return None;
        }
        let dir = self.paths.data_dir.join("attachments");
        std::fs::create_dir_all(&dir).ok()?;
        let stamp = Utc::now().format("%Y%m%d-%H%M%S");
        let unique = Uuid::new_v4().simple().to_string();
        let name = format!("pasted-{stamp}-{}.{ext}", &unique[..8]);
        let path = dir.join(name);
        std::fs::write(&path, &image.bytes).ok()?;
        Some(path)
    }

    fn cancel_current(&mut self) {
        if let Some(sid) = self.selected_session_id.clone() {
            self.cancel_session_by_id(sid);
        }
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

    fn set_color_theme(
        &mut self,
        color_theme: ColorTheme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.config.color_theme == color_theme {
            return;
        }
        self.config.color_theme = color_theme;
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
                let is_skill = skills::detect_dollar_skill_token(&value).is_some();
                // Suggestions filter on every keystroke while a command or `$skill`
                // token is typed, so notify for those too.
                if empty != self.prompt_empty
                    || is_command
                    || is_command != self.prompt_is_command
                    || is_skill
                    || is_skill != self.prompt_is_skill_mention
                {
                    self.prompt_empty = empty;
                    self.prompt_is_command = is_command;
                    self.prompt_is_skill_mention = is_skill;
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
        self.prompt_input.update(cx, |ta, cx| {
            ta.set_submit_on_enter(key == SubmitKey::Enter, cx)
        });
        self.apply_placeholders(window, cx);
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

fn status_color(status: SessionStatus, theme: &Theme) -> Hsla {
    match status {
        SessionStatus::Running | SessionStatus::WaitingForSubagent => accent_indicator().into(),
        SessionStatus::WaitingForApproval | SessionStatus::WaitingForUserInput => {
            status_amber().into()
        }
        SessionStatus::Completed => status_green().into(),
        SessionStatus::Failed => status_red().into(),
        SessionStatus::Interrupted | SessionStatus::Cancelled => theme.muted_foreground,
        SessionStatus::Idle => theme.muted_foreground.opacity(0.55),
    }
}

/// Separates a failed run's error (stored after [`ERROR_MARKER`]) from the reply.
fn split_error(content: &str) -> (&str, Option<&str>) {
    match content.split_once(ERROR_MARKER) {
        Some((body, err)) => (body.trim_end(), Some(err)),
        None => (content, None),
    }
}

/// Builds a compact 1-line status badge + summary label for a turn's tool calls
/// in the main chat feed, keeping full inputs/outputs in the right-pane Tools view.
fn summarize_tool_calls(
    tool_calls: &[ToolCallRecord],
    lang: Language,
) -> (&'static str, Hsla, String) {
    let total = tool_calls.len();
    let in_progress = tool_calls
        .iter()
        .filter(|tc| tc.status == ToolCallStatus::InProgress)
        .count();
    let pending = tool_calls
        .iter()
        .filter(|tc| tc.status == ToolCallStatus::Pending)
        .count();
    let failed = tool_calls
        .iter()
        .filter(|tc| tc.status == ToolCallStatus::Failed)
        .count();
    let denied = tool_calls
        .iter()
        .filter(|tc| tc.status == ToolCallStatus::Denied)
        .count();

    let title_for = |status: ToolCallStatus| -> String {
        let raw = tool_calls
            .iter()
            .rev()
            .find(|tc| tc.status == status)
            .map(|tc| {
                let t = tc.title.trim();
                if t.is_empty() {
                    tc.kind.as_deref().unwrap_or("")
                } else {
                    t
                }
            })
            .unwrap_or("");
        raw.split_whitespace().collect::<Vec<_>>().join(" ")
    };

    if in_progress > 0 {
        let active_title = title_for(ToolCallStatus::InProgress);
        let label = if active_title.is_empty() {
            match lang {
                Language::Ja => format!("ツール実行中 ({in_progress}/{total}件)"),
                Language::En => format!("Running {in_progress}/{total} tools"),
            }
        } else {
            match lang {
                Language::Ja => format!("ツール実行中 ({in_progress}/{total}件): {active_title}"),
                Language::En => format!("Running {in_progress}/{total}: {active_title}"),
            }
        };
        ("◐", accent_text().into(), label)
    } else if pending > 0 {
        let waiting_title = title_for(ToolCallStatus::Pending);
        let label = if waiting_title.is_empty() {
            match lang {
                Language::Ja => format!("ツール待機中 ({pending}/{total}件)"),
                Language::En => format!("Waiting {pending}/{total} tools"),
            }
        } else {
            match lang {
                Language::Ja => format!("ツール待機中 ({pending}/{total}件): {waiting_title}"),
                Language::En => format!("Waiting {pending}/{total}: {waiting_title}"),
            }
        };
        ("○", status_amber().into(), label)
    } else if failed > 0 && denied > 0 {
        let label = match lang {
            Language::Ja => format!("ツール {total}件（失敗 {failed} / 拒否 {denied}）"),
            Language::En => format!("{total} tools ({failed} failed, {denied} denied)"),
        };
        ("✗", status_red().into(), label)
    } else if failed > 0 {
        let label = match lang {
            Language::Ja => format!("{total}件のツールを実行（失敗 {failed}）"),
            Language::En => {
                if total == 1 {
                    "Ran 1 tool (1 failed)".to_string()
                } else {
                    format!("Ran {total} tools ({failed} failed)")
                }
            }
        };
        ("✗", status_red().into(), label)
    } else if denied > 0 {
        let label = if denied == total {
            match lang {
                Language::Ja => format!("{total}件のツールを拒否"),
                Language::En => {
                    if total == 1 {
                        "1 tool denied".to_string()
                    } else {
                        format!("{total} tools denied")
                    }
                }
            }
        } else {
            match lang {
                Language::Ja => format!("ツール {total}件（拒否 {denied}）"),
                Language::En => format!("{total} tools ({denied} denied)"),
            }
        };
        ("⊘", status_red().into(), label)
    } else {
        let label = match lang {
            Language::Ja => format!("{total}件のツールを実行"),
            Language::En => {
                if total == 1 {
                    "Ran 1 tool".to_string()
                } else {
                    format!("Ran {total} tools")
                }
            }
        };
        ("✓", status_green().into(), label)
    }
}

impl SameAppView {
    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let lang = self.config.language;
        let skills_button = title_icon_button(
            "sidebar-skills",
            IconName::BookOpen,
            self.active_view == MainView::Skills,
            &theme,
        )
        .tooltip(move |window, cx| {
            gpui_component::tooltip::Tooltip::new(t(lang, "スキル管理", "Skills Manager"))
                .build(window, cx)
        })
        .on_click(cx.listener(|this, _, _, cx| {
            if this.active_view == MainView::Skills {
                this.active_view = MainView::Chat;
            } else {
                this.open_skills_view();
            }
            cx.notify();
        }));

        // The one way into the settings (and back out).
        let settings_button = title_icon_button(
            "sidebar-settings",
            IconName::Settings,
            self.active_view == MainView::Settings,
            &theme,
        )
        .tooltip(move |window, cx| {
            gpui_component::tooltip::Tooltip::new(t(lang, "設定", "Settings")).build(window, cx)
        })
        .on_click(cx.listener(|this, _, _, cx| {
            if this.active_view == MainView::Settings {
                this.active_view = MainView::Chat;
            } else {
                let section = this.settings_section;
                this.open_settings(section);
            }
            cx.notify();
        }));

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
            .when_some(self.render_running_sessions_section(cx), |el, sec| {
                el.child(sec)
            })
            .child(self.render_projects_section(cx))
            .child(div().h(px(1.0)).bg(theme.border))
            .child(self.render_quests_section(cx))
            .child(div().h(px(1.0)).bg(theme.border))
            .child(self.render_sessions_section(cx))
            .child(div().h(px(1.0)).bg(theme.border))
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .justify_between()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(
                        div()
                            .h_flex()
                            .items_center()
                            .gap_1()
                            .child(settings_button)
                            .child(skills_button)
                            .child(format!("SAME {}", updater::display_version())),
                    )
                    .when_some(
                        match &self.update_state {
                            UpdateState::Available(info) => Some(format!("↑ v{}", info.version)),
                            UpdateState::Installed(ver) => Some(match lang {
                                Language::Ja => format!("再起動 (v{ver})"),
                                Language::En => format!("Restart (v{ver})"),
                            }),
                            _ => None,
                        },
                        |el, badge| {
                            el.child(
                                div()
                                    .id("sidebar-update-badge")
                                    .px_1p5()
                                    .py(px(1.0))
                                    .rounded(px(4.0))
                                    .bg(accent())
                                    .text_color(accent_foreground())
                                    .font_weight(FontWeight::MEDIUM)
                                    .cursor_pointer()
                                    .child(badge)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.open_settings(SettingsSection::About);
                                        cx.notify();
                                    })),
                            )
                        },
                    ),
            )
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
                    .children({
                        let mut quest_counts: HashMap<&str, usize> =
                            HashMap::with_capacity(self.quests.len());
                        for s in &self.sessions {
                            if let Some(qid) = s.quest_id.as_deref() {
                                *quest_counts.entry(qid).or_insert(0) += 1;
                            }
                        }
                        self.quests.iter().map(move |q| {
                            let qid = q.id.clone();
                            let qid_del = q.id.clone();
                            let selected =
                                self.selected_quest_filter.as_deref() == Some(q.id.as_str());
                            let count = quest_counts.get(q.id.as_str()).copied().unwrap_or(0);
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
                                    if this.selected_quest_filter.as_deref()
                                        == Some(qid_del.as_str())
                                    {
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
                        })
                    }),
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
                        let sid_stop = s.id.clone();
                        let selected = self.selected_session_id.as_deref() == Some(s.id.as_str());
                        let running = self.is_session_running(&s.id);
                        let waiting_perm = self
                            .pending_permissions
                            .iter()
                            .any(|p| p.session_id == s.id);
                        let waiting_input = self
                            .pending_inputs
                            .iter()
                            .any(|p| p.request.session_id == s.id);
                        let unread = self
                            .session_ui_states
                            .get(&s.id)
                            .is_some_and(|st| st.has_unread_completion || st.has_unread_error);
                        let renaming = self.renaming_session_id.as_deref() == Some(s.id.as_str());
                        let dot = if waiting_perm || waiting_input {
                            status_amber().into()
                        } else if running {
                            accent_indicator().into()
                        } else if unread {
                            status_green().into()
                        } else {
                            status_color(s.status, &theme)
                        };
                        let wd_badge = match self.session_workdir_mode(&s.id) {
                            WorkdirMode::Isolated => " ⎇",
                            WorkdirMode::ReadOnly => " 🔒",
                            WorkdirMode::Shared => "",
                        };
                        let model_tag = s
                            .model
                            .as_deref()
                            .or_else(|| self.agent_current_model.get(&s.id).map(String::as_str))
                            .map(|m| format!("/{m}"))
                            .unwrap_or_default();

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
                                .child(
                                    div()
                                        .truncate()
                                        .child(format!("{}{wd_badge}", self.session_title(s))),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .truncate()
                                        .text_color(theme.muted_foreground)
                                        .child(if waiting_perm {
                                            t(lang, "承認待ち", "Needs approval").to_string()
                                        } else if waiting_input {
                                            t(lang, "質問待ち", "Needs input").to_string()
                                        } else if running {
                                            format!(
                                                "{} · {}",
                                                t(lang, "応答中…", "Responding…"),
                                                self.provider_name(&s.provider_id)
                                            )
                                        } else {
                                            format!(
                                                "{}{model_tag} · {}",
                                                self.provider_name(&s.provider_id),
                                                rel_time(s.updated_at, lang)
                                            )
                                        }),
                                ),
                        )
                        .when(running, |el| {
                            el.child(
                                Button::new(SharedString::from(format!("stop-sess-{}", s.id)))
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
                        })
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
                            this.invalidate_and_refresh_quotas(None);
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
            let auth_warning = if chk.available {
                match chk.auth_status {
                    ProviderAuthStatus::NotLoggedIn => Some(t(lang, "未ログイン", "Not logged in")),
                    ProviderAuthStatus::NoSubscription => {
                        Some(t(lang, "サブスクリプションなし", "No active subscription"))
                    }
                    ProviderAuthStatus::LoggedIn | ProviderAuthStatus::Unknown => None,
                }
            } else {
                None
            };
            let color = if !chk.available || auth_warning.is_some() {
                status_amber()
            } else {
                status_green()
            };
            let base = if let Some(prefix) = auth_warning {
                format!("{prefix} · {}", chk.detail)
            } else {
                chk.detail.clone()
            };
            (
                color,
                if cfg!(windows) {
                    format!("{base} · {}", chk.target)
                } else {
                    base
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
                let in_app_active = self
                    .active_login
                    .as_ref()
                    .is_some_and(|(_, pid, _)| pid == &p.id);
                let auth_url = in_app_active.then(|| extract_first_url(&msg)).flatten();
                let pid_cancel = p.id.clone();
                el.child(
                    div()
                        .v_flex()
                        .gap_2()
                        .text_xs()
                        .p_2()
                        .rounded(px(4.0))
                        .bg(theme.background)
                        .border_1()
                        .border_color(gpui::Hsla::from(accent()))
                        .child(msg)
                        .when_some(auth_url, |box_el, url| {
                            box_el.child(
                                div().h_flex().gap_2().child(
                                    Button::new(SharedString::from(format!(
                                        "open-login-url-{}",
                                        p.id
                                    )))
                                    .xsmall()
                                    .ghost()
                                    .label(t(lang, "ブラウザで開く", "Open in browser"))
                                    .on_click(move |_, _, cx| cx.open_url(&url)),
                                ),
                            )
                        })
                        .when(in_app_active, |box_el| {
                            box_el.child(
                                div()
                                    .h_flex()
                                    .gap_2()
                                    .items_center()
                                    .child(
                                        div()
                                            .flex_1()
                                            .child(Input::new(&self.login_code_input).small()),
                                    )
                                    .child(
                                        Button::new(SharedString::from(format!(
                                            "submit-login-code-{}",
                                            p.id
                                        )))
                                        .small()
                                        .primary()
                                        .label(t(lang, "送信", "Submit"))
                                        .on_click(
                                            cx.listener(|this, _, window, cx| {
                                                this.submit_in_app_login_code(window, cx);
                                                cx.notify();
                                            }),
                                        ),
                                    )
                                    .child(
                                        Button::new(SharedString::from(format!(
                                            "cancel-login-code-{}",
                                            p.id
                                        )))
                                        .small()
                                        .ghost()
                                        .label(t(lang, "キャンセル", "Cancel"))
                                        .on_click(
                                            cx.listener(move |this, _, _, cx| {
                                                this.cancel_in_app_login(&pid_cancel);
                                                cx.notify();
                                            }),
                                        ),
                                    ),
                            )
                        }),
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
                                            this.invalidate_and_refresh_quotas(Some(&pid_save));
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
                    .child({
                        let in_app_login = AgentManager::supports_in_app_login(p);
                        Button::new(SharedString::from(format!("login-{}", p.id)))
                            .small()
                            .ghost()
                            .disabled(setup_running)
                            .tooltip(login_preview)
                            .label(if setup_running {
                                if in_app_login {
                                    t(lang, "ログイン中…", "Signing in…")
                                } else {
                                    t(lang, "ターミナルで操作中…", "Waiting for terminal…")
                                }
                            } else if in_app_login {
                                t(lang, "ログイン", "Sign in")
                            } else {
                                t(lang, "ログイン (ターミナル)", "Sign in (terminal)")
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                if in_app_login {
                                    this.start_in_app_login(p_login.clone(), window, cx);
                                } else {
                                    this.run_provider_setup(p_login.clone(), ProviderSetup::Login);
                                }
                                cx.notify();
                            }))
                    })
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
            SettingsSection::About => self.render_about_settings(cx),
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

        let language_control = div().h_flex().gap_1().children(
            [(Language::Ja, "日本語"), (Language::En, "English")].map(|(value, label)| {
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
            }),
        );

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

        let default_mode = self.config.default_approval_mode;
        let default_mode_control =
            div()
                .h_flex()
                .gap_1()
                .flex_wrap()
                .children(ApprovalMode::ALL.map(|mode| {
                    Button::new(SharedString::from(format!(
                        "set-default-mode-{}",
                        mode.as_str()
                    )))
                    .small()
                    .label(approvals::mode_label(lang, mode))
                    .tooltip(approvals::mode_hint(lang, mode))
                    .toggled(default_mode == mode)
                    .when(default_mode == mode, |b| {
                        if mode == ApprovalMode::FullAccess {
                            b.warning()
                        } else {
                            b.primary()
                        }
                    })
                    .when(default_mode != mode, |b| b.ghost())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.config.default_approval_mode = mode;
                        this.save_config();
                        cx.notify();
                    }))
                }));
        let notifications = self.config.notifications;
        let notifications_control = div().h_flex().gap_1().children([true, false].map(|on| {
            Button::new(SharedString::from(format!("set-notifications-{on}")))
                .small()
                .label(if on {
                    t(lang, "オン", "On")
                } else {
                    t(lang, "オフ", "Off")
                })
                .toggled(notifications == on)
                .when(notifications == on, |b| b.primary())
                .when(notifications != on, |b| b.ghost())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.config.notifications = on;
                    this.save_config();
                    cx.notify();
                }))
        }));

        let submit_key = self.config.submit_key;
        let mod_key = if cfg!(target_os = "macos") {
            "Cmd"
        } else {
            "Ctrl"
        };
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

        let max_concurrent = self.config.max_concurrent_sessions();
        let concurrency_label = match max_concurrent {
            Some(n) => format!("{n}"),
            None => t(lang, "無制限", "Unlimited").to_string(),
        };
        let concurrency_control = div()
            .h_flex()
            .items_center()
            .gap_1()
            .child(
                Button::new("set-concurrency-unlimited")
                    .small()
                    .label(t(lang, "無制限", "Unlimited"))
                    .toggled(max_concurrent.is_none())
                    .when(max_concurrent.is_none(), |b| b.primary())
                    .when(max_concurrent.is_some(), |b| b.ghost())
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.config.max_concurrent_sessions = None;
                        this.agent_manager.set_max_concurrent_sessions(None);
                        this.save_config();
                        cx.notify();
                    })),
            )
            .child(
                Button::new("set-concurrency-dec")
                    .small()
                    .ghost()
                    .disabled(max_concurrent.is_none())
                    .label("−")
                    .on_click(cx.listener(|this, _, _, cx| {
                        let next = match this.config.max_concurrent_sessions() {
                            Some(n) if n > MIN_CONCURRENT_SESSIONS => Some(n - 1),
                            _ => None,
                        };
                        this.config.max_concurrent_sessions = next;
                        this.agent_manager.set_max_concurrent_sessions(next);
                        this.save_config();
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .px_2()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .child(concurrency_label),
            )
            .child(
                Button::new("set-concurrency-inc")
                    .small()
                    .ghost()
                    .disabled(max_concurrent.is_some_and(|n| n >= MAX_CONCURRENT_SESSIONS))
                    .label("+")
                    .on_click(cx.listener(|this, _, _, cx| {
                        let next = match this.config.max_concurrent_sessions() {
                            None => Some(4),
                            Some(n) => Some((n + 1).min(MAX_CONCURRENT_SESSIONS)),
                        };
                        this.config.max_concurrent_sessions = next;
                        this.agent_manager.set_max_concurrent_sessions(next);
                        this.save_config();
                        cx.notify();
                    })),
            );

        let default_handoff = self.config.default_handoff_mode;
        let handoff_control = div()
            .h_flex()
            .gap_1()
            .flex_wrap()
            .children(HandoffMode::ALL.map(|mode| {
                Button::new(SharedString::from(format!(
                    "set-default-handoff-{}",
                    mode.as_str()
                )))
                .small()
                .label(parallel::handoff_mode_label(lang, mode))
                .toggled(default_handoff == mode)
                .when(default_handoff == mode, |b| b.primary())
                .when(default_handoff != mode, |b| b.ghost())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.config.default_handoff_mode = mode;
                    this.save_config();
                    cx.notify();
                }))
            }));

        let default_workdir = self.config.default_workdir_mode;
        let workdir_control = div()
            .h_flex()
            .gap_1()
            .flex_wrap()
            .children(WorkdirMode::ALL.map(|mode| {
                Button::new(SharedString::from(format!(
                    "set-default-workdir-{}",
                    mode.as_str()
                )))
                .small()
                .label(parallel::workdir_mode_label(lang, mode))
                .toggled(default_workdir == mode)
                .when(default_workdir == mode, |b| b.primary())
                .when(default_workdir != mode, |b| b.ghost())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.config.default_workdir_mode = mode;
                    this.save_config();
                    cx.notify();
                }))
            }));

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
                        t(lang, "承認モードの既定", "Default approval mode"),
                        t(
                            lang,
                            "新しいセッションの承認モード。各セッションでは入力欄の下で切り替えられます",
                            "For sessions that have not picked one; switch per session under the prompt",
                        ),
                        default_mode_control,
                        false,
                        &theme,
                    ))
                    .child(settings_row(
                        t(lang, "通知", "Notifications"),
                        t(
                            lang,
                            "ウィンドウが背面にある間、承認待ち・質問・完了を OS の通知で知らせます",
                            "While the window is in the background, OS notifications for approvals, questions and finished replies",
                        ),
                        notifications_control,
                        false,
                        &theme,
                    ))
                    .child(settings_row(
                        t(lang, "並列実行数の上限", "Max concurrent sessions"),
                        t(
                            lang,
                            "同時に実行できるセッション数の上限（既定は無制限、必要時のみ 1〜16 で設定）",
                            "Optional cap on concurrently running sessions (unlimited by default, or 1–16)",
                        ),
                        concurrency_control,
                        false,
                        &theme,
                    ))
                    .child(settings_row(
                        t(lang, "既定のコンテキスト引き継ぎ", "Default context handoff"),
                        t(
                            lang,
                            "別プロバイダへ切り替える際の既定の引き継ぎ方式",
                            "Default strategy when handing off context to another provider",
                        ),
                        handoff_control,
                        false,
                        &theme,
                    ))
                    .child(settings_row(
                        t(lang, "新規セッションの既定の作業環境", "Default working environment"),
                        t(
                            lang,
                            "新しいセッション作成時の作業環境（分離は Git リポジトリのみ自動作成、非 Git では共有を使用）",
                            "Working environment for new sessions (Isolated auto-creates a worktree in Git repos, falls back to Shared otherwise)",
                        ),
                        workdir_control,
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

    /// Settings → About: app identity, build version and updates.
    fn render_about_settings(&self, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme().clone();
        let lang = self.config.language;
        let version = updater::display_version();

        let build_kind = if updater::IS_RELEASE_BUILD {
            t(lang, "リリースビルド", "Release build")
        } else {
            t(lang, "開発ビルド", "Development build")
        };
        let copy_text = format!(
            "SAME {version}\n{} {} ({})",
            std::env::consts::OS,
            std::env::consts::ARCH,
            build_kind
        );

        let identity = div()
            .h_flex()
            .items_center()
            .gap_4()
            .px_4()
            .py_4()
            .child(gpui::img(crate::brand::shark_egg()).size(px(64.0)))
            .child(
                div()
                    .v_flex()
                    .gap_1()
                    .min_w_0()
                    .flex_1()
                    .child(
                        div()
                            .text_xl()
                            .font_weight(FontWeight::BOLD)
                            .text_color(accent_text())
                            .child("SAME"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child("Shared Agentic Multi-provider Environment"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .child(version.clone()),
                    ),
            )
            .child(
                Button::new("about-copy-version")
                    .small()
                    .ghost()
                    .label(t(lang, "コピー", "Copy"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copy_text.clone()));
                        this.note(
                            NoteKind::Info,
                            t(this.config.language, "コピーしました", "Copied"),
                        );
                        cx.notify();
                    })),
            );

        let repo_url = format!("https://github.com/{}", updater::GITHUB_REPO);
        let repo_button = Button::new("about-open-repo")
            .small()
            .outline()
            .label("GitHub")
            .on_click({
                let url = repo_url.clone();
                move |_, _, cx| cx.open_url(&url)
            });
        let releases_url = format!("{repo_url}/releases");
        let releases_button = Button::new("about-open-releases")
            .small()
            .outline()
            .label(t(lang, "リリースノート", "Release notes"))
            .on_click(move |_, _, cx| cx.open_url(&releases_url));

        div()
            .v_flex()
            .gap_4()
            .child(settings_header(
                SettingsSection::About.title(lang),
                t(
                    lang,
                    "バージョン情報とアップデート",
                    "Version information and updates",
                ),
                &theme,
                None,
            ))
            .child(settings_card(&theme).child(identity))
            .child(
                settings_card(&theme)
                    .child(settings_row(
                        t(lang, "バージョン", "Version"),
                        &format!(
                            "{build_kind} · {} {}",
                            std::env::consts::OS,
                            std::env::consts::ARCH
                        ),
                        div().text_sm().child(version),
                        true,
                        &theme,
                    ))
                    .child(self.render_update_row(cx))
                    .child(settings_row(
                        t(lang, "ソースコード", "Source code"),
                        &repo_url,
                        div()
                            .h_flex()
                            .gap_1()
                            .child(releases_button)
                            .child(repo_button),
                        false,
                        &theme,
                    ))
                    .child(settings_row(
                        t(lang, "ライセンス", "License"),
                        "",
                        div().text_sm().child("MIT OR Apache-2.0"),
                        false,
                        &theme,
                    )),
            )
    }

    /// The "check / install update" row of Settings → About.
    fn render_update_row(&self, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme().clone();
        let lang = self.config.language;
        let (update_desc, update_control): (String, AnyElement) = match &self.update_state {
            UpdateState::Idle => (
                t(
                    lang,
                    "GitHub Releases で新しいバージョンを確認します",
                    "Check GitHub Releases for a newer version",
                )
                .to_string(),
                Button::new("settings-check-update")
                    .small()
                    .outline()
                    .label(t(lang, "アップデートを確認", "Check for updates"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.check_for_updates(true);
                        cx.notify();
                    }))
                    .into_any_element(),
            ),
            UpdateState::Checking => (
                t(
                    lang,
                    "最新バージョンを確認しています...",
                    "Checking for updates...",
                )
                .to_string(),
                Button::new("settings-check-update")
                    .small()
                    .outline()
                    .disabled(true)
                    .label(t(lang, "確認中...", "Checking..."))
                    .into_any_element(),
            ),
            UpdateState::UpToDate => (
                match lang {
                    Language::Ja => {
                        format!("v{CURRENT_VERSION} は最新です")
                    }
                    Language::En => {
                        format!("v{CURRENT_VERSION} is up to date")
                    }
                },
                Button::new("settings-check-update")
                    .small()
                    .outline()
                    .label(t(lang, "再確認", "Check again"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.check_for_updates(true);
                        cx.notify();
                    }))
                    .into_any_element(),
            ),
            UpdateState::Available(info) => {
                let info_clone = info.clone();
                let has_asset = info.download_url.is_some() && info.checksums_url.is_some();
                let desc = match lang {
                    Language::Ja => format!("現在: v{CURRENT_VERSION} → 最新: v{}", info.version),
                    Language::En => {
                        format!("Current: v{CURRENT_VERSION} → Latest: v{}", info.version)
                    }
                };
                let ctrl = div()
                    .h_flex()
                    .gap_1()
                    .when(has_asset, |d| {
                        let ver_label = match lang {
                            Language::Ja => format!("v{} にアップデート", info.version),
                            Language::En => format!("Update to v{}", info.version),
                        };
                        d.child(
                            Button::new("settings-install-update")
                                .small()
                                .primary()
                                .label(ver_label)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.install_update(info_clone.clone());
                                    cx.notify();
                                })),
                        )
                    })
                    .child(
                        Button::new("settings-recheck-update")
                            .small()
                            .ghost()
                            .label(t(lang, "再確認", "Recheck"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.check_for_updates(true);
                                cx.notify();
                            })),
                    )
                    .into_any_element();
                (desc, ctrl)
            }
            UpdateState::Downloading(info) => (
                match lang {
                    Language::Ja => {
                        format!("v{} をダウンロードして適用しています...", info.version)
                    }
                    Language::En => format!("Downloading and installing v{}...", info.version),
                },
                Button::new("settings-downloading-update")
                    .small()
                    .outline()
                    .disabled(true)
                    .label(t(lang, "アップデート中...", "Updating..."))
                    .into_any_element(),
            ),
            UpdateState::Installed(ver) => (
                match lang {
                    Language::Ja => format!("v{ver} の準備ができました。再起動して適用します"),
                    Language::En => format!("v{ver} is ready. Restart to apply"),
                },
                Button::new("settings-restart-update")
                    .small()
                    .primary()
                    .label(t(lang, "再起動して適用", "Restart to apply"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if this.file_dirty {
                            this.pending_notes.push((
                                NoteKind::Warning,
                                t(
                                    this.config.language,
                                    "未保存のファイルがあります。保存してから再起動してください",
                                    "You have unsaved file changes. Save them before restarting",
                                )
                                .to_string(),
                            ));
                            cx.notify();
                            return;
                        }
                        if let Err(err) = updater::restart_self() {
                            this.pending_notes
                                .push((NoteKind::Error, format!("Restart failed: {err:#}")));
                            cx.notify();
                        } else {
                            cx.quit();
                        }
                    }))
                    .into_any_element(),
            ),
            UpdateState::Error(err) => (
                match lang {
                    Language::Ja => format!("エラー: {err}"),
                    Language::En => format!("Error: {err}"),
                },
                Button::new("settings-retry-update")
                    .small()
                    .outline()
                    .label(t(lang, "再試行", "Retry"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.check_for_updates(true);
                        cx.notify();
                    }))
                    .into_any_element(),
            ),
        };

        settings_row(
            t(lang, "アップデート", "Updates"),
            &update_desc,
            update_control,
            false,
            &theme,
        )
    }

    fn render_appearance_settings(&self, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme().clone();
        let lang = self.config.language;

        let mode_control = div().h_flex().gap_1().children(
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

        let mode = self.config.theme;
        let theme_cards =
            div()
                .h_flex()
                .flex_wrap()
                .gap_2()
                .children(ColorTheme::ALL.into_iter().map(|value| {
                    let selected = self.config.color_theme == value;
                    let (ja, en) = value.label();
                    let pal = palette(value, mode);
                    let rgb = |hex: u32| -> Hsla { gpui::rgb(hex).into() };
                    // A miniature window: sidebar strip, a text line and the
                    // primary colour, drawn in the theme's own colours.
                    let swatch = div()
                        .h_flex()
                        .w_full()
                        .h(px(44.0))
                        .rounded(px(6.0))
                        .overflow_hidden()
                        .border_1()
                        .border_color(rgb(pal.surface))
                        .bg(rgb(pal.background))
                        .child(div().w(px(18.0)).h_full().bg(rgb(pal.surface)))
                        .child(
                            div()
                                .flex_1()
                                .v_flex()
                                .justify_center()
                                .gap_1()
                                .px_2()
                                .child(
                                    div()
                                        .text_xs()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(rgb(pal.foreground))
                                        .child("Aa"),
                                )
                                .child(
                                    div()
                                        .h(px(6.0))
                                        .w(px(36.0))
                                        .rounded_full()
                                        .bg(rgb(pal.primary)),
                                ),
                        )
                        .when(selected, |el| {
                            el.child(
                                div()
                                    .size(px(18.0))
                                    .mr_1p5()
                                    .rounded_full()
                                    .bg(rgb(pal.primary))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .text_color(rgb(pal.primary_foreground))
                                    .child(Icon::new(IconName::Check).xsmall()),
                            )
                        });
                    Button::new(SharedString::from(format!("set-color-theme-{en}")))
                        .ghost()
                        .selected(selected)
                        .toggled(selected)
                        .h_auto()
                        .w(px(120.0))
                        .p_1p5()
                        .rounded(px(8.0))
                        .border_1()
                        .border_color(if selected { theme.ring } else { theme.border })
                        .child(
                            div()
                                .v_flex()
                                .w_full()
                                .gap_1p5()
                                .child(swatch)
                                .child(div().text_xs().child(t(lang, ja, en))),
                        )
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.set_color_theme(value, window, cx);
                            cx.notify();
                        }))
                }));

        let preview = div()
            .h_flex()
            .items_center()
            .gap_2()
            .child(
                Button::new("accent-preview-primary")
                    .small()
                    .primary()
                    .label(t(lang, "プライマリ", "Primary")),
            )
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
                    "テーマ・モード・フォントを変更します",
                    "Theme, light/dark mode and fonts",
                ),
                &theme,
                None,
            ))
            .child(
                settings_card(&theme)
                    .child(settings_row(
                        t(lang, "モード", "Mode"),
                        t(
                            lang,
                            "テーマのダーク / ライトを切り替えます",
                            "Dark or light variant of the theme",
                        ),
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
                                        div().text_sm().font_weight(FontWeight::MEDIUM).child(t(
                                            lang,
                                            "テーマ",
                                            "Theme",
                                        )),
                                    )
                                    .child(
                                        div().text_xs().text_color(theme.muted_foreground).child(
                                            t(
                                                lang,
                                                "背景・文字・ボタンなどの配色。各テーマにダークとライトがあります",
                                                "Colours for backgrounds, text and buttons. Each theme has a dark and a light variant",
                                            ),
                                        ),
                                    ),
                            )
                            .child(theme_cards)
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
                t(
                    lang,
                    "メニュー・チャット・ボタンなど",
                    "Menus, chat, buttons and so on",
                ),
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
                            .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(label))
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
                list_row(
                    SharedString::from(format!("font-opt-{ix}")),
                    selected,
                    &theme,
                )
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
                            .child(self.render_workdir_header_menu(session, running, cx))
                            .child({
                                let active_n = self.skills_ui.resolution.active_skills.len();
                                let ext_n = self.skills_ui.external_skills.len();
                                let label = if active_n > 0 {
                                    format!("Skills ({active_n})")
                                } else {
                                    "Skills".to_string()
                                };
                                Button::new("chat-header-skills")
                                    .xsmall()
                                    .ghost()
                                    .tooltip(match lang {
                                        Language::Ja => format!(
                                            "有効なスキル: {active_n} 件 / 外部スキル: {ext_n} 件（クリックで管理画面を開く）"
                                        ),
                                        Language::En => format!(
                                            "Active skills: {active_n} / External: {ext_n} (click to manage)"
                                        ),
                                    })
                                    .label(label)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.open_skills_view();
                                        cx.notify();
                                    }))
                            })
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
        self.agent_manager.release_session(sid);
    }

    /// The row under the prompt: agent and model pickers (like the Codex /
    /// Claude Code composers), the auto-approve toggle, and key hints.
    /// Returns whether `provider` should be shown in workspace pickers
    /// (enabled, CLI installed, and logged in with an active subscription when
    /// auth status is known).
    pub(super) fn is_provider_visible_in_workspace(&self, provider: &ProviderConfig) -> bool {
        if !provider.enabled {
            return false;
        }
        if let Some(check) = self.provider_checks.get(&provider.id) {
            if !check.available || !check.auth_status.is_usable() {
                return false;
            }
        }
        true
    }

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

        // Agent picker (workspace principle: show only usable providers;
        // quota-exhausted ones are shown disabled with remaining quota).
        let providers: Vec<(String, String, Option<String>)> = self
            .config
            .providers
            .iter()
            .filter(|p| self.is_provider_visible_in_workspace(p))
            .map(|p| {
                let exhausted_suffix = self
                    .provider_quota_summary(&p.id)
                    .filter(|q| q.exhausted)
                    .map(|q| Self::format_exhausted_quota_suffix(&q, lang));
                (p.id.clone(), p.name.clone(), exhausted_suffix)
            })
            .collect();
        let active_pid = session.provider_id.clone();
        let active_check = self.provider_checks.get(&session.provider_id);
        let active_exhausted = self
            .provider_quota_summary(&session.provider_id)
            .is_some_and(|q| q.exhausted);
        let dot: Hsla = match active_check {
            Some(c) if !c.available || !c.auth_status.is_usable() => status_red().into(),
            Some(_) if active_exhausted => status_amber().into(),
            Some(_) => status_green().into(),
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
                if providers.is_empty() {
                    menu = menu.item(
                        PopupMenuItem::new(t(
                            lang,
                            "利用可能なプロバイダがありません",
                            "No available providers",
                        ))
                        .disabled(true),
                    );
                } else {
                    for (pid, name, exhausted_suffix) in providers.iter() {
                        if let Some(suffix) = exhausted_suffix {
                            menu = menu.item(
                                PopupMenuItem::new(format!("{name}  {suffix}"))
                                    .checked(*pid == active_pid)
                                    .disabled(true),
                            );
                        } else {
                            let v = provider_view.clone();
                            let pid2 = pid.clone();
                            let sid2 = provider_sid.clone();
                            menu = menu.item(
                                PopupMenuItem::new(name.clone())
                                    .checked(*pid == active_pid)
                                    .on_click(move |_, _, cx| {
                                        let pid3 = pid2.clone();
                                        let sid3 = sid2.clone();
                                        let _ = v.update(cx, |this, cx| {
                                            this.request_provider_switch(&sid3, &pid3);
                                            cx.notify();
                                        });
                                    }),
                            );
                        }
                    }
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

        let plus_view = cx.entity().downgrade();
        let attach_button = Button::new("composer-attach")
            .small()
            .ghost()
            .icon(IconName::Plus)
            .rounded(ButtonRounded::Size(px(999.0)))
            .tooltip(t(
                lang,
                "ファイルを追加・スキルを選択 ($スキル名 でも補完できます)",
                "Attach files or select skills (or type $skill-name)",
            ))
            .dropdown_menu(move |mut menu, _, _| {
                let v_file = plus_view.clone();
                menu = menu.item(
                    PopupMenuItem::new(t(lang, "📎 ファイルを追加...", "📎 Attach files..."))
                        .on_click(move |_, window, cx| {
                            let _ = v_file.update(cx, |this, cx| {
                                this.show_skill_picker = false;
                                this.pick_attachments(window, cx);
                                cx.notify();
                            });
                        }),
                );
                let v_skill = plus_view.clone();
                menu = menu.item(
                    PopupMenuItem::new(t(lang, "✦ スキルを選択...", "✦ Select skills..."))
                        .on_click(move |_, window, cx| {
                            let _ = v_skill.update(cx, |this, cx| {
                                this.show_skill_picker = true;
                                this.skill_picker_search.update(cx, |i, cx| {
                                    i.set_value("", window, cx);
                                    i.focus(window, cx);
                                });
                                cx.notify();
                            });
                        }),
                );
                menu = menu.separator();
                let v_mgr = plus_view.clone();
                menu = menu.item(
                    PopupMenuItem::new(t(lang, "⚙ スキル管理を開く...", "⚙ Manage Skills..."))
                        .on_click(move |_, _, cx| {
                            let _ = v_mgr.update(cx, |this, cx| {
                                this.show_skill_picker = false;
                                this.open_skills_view();
                                cx.notify();
                            });
                        }),
                );
                menu
            });

        let can_send = !self.prompt_empty
            || !self.attachments.is_empty()
            || !self.selected_prompt_skills.is_empty();
        let submit_hint = match self.config.submit_key {
            SubmitKey::Enter => "Enter",
            SubmitKey::ModEnter => {
                if cfg!(target_os = "macos") {
                    "Cmd+Enter"
                } else {
                    "Ctrl+Enter"
                }
            }
        };
        let action_button = if running {
            Button::new("btn-cancel-prompt")
                .small()
                .icon(IconName::Square)
                .rounded(ButtonRounded::Size(px(999.0)))
                .tooltip(format!("{} (Esc)", t(lang, "停止", "Stop")))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.cancel_current();
                    cx.notify();
                }))
        } else {
            Button::new("btn-send-prompt")
                .small()
                .primary()
                .icon(IconName::ArrowUp)
                .rounded(ButtonRounded::Size(px(999.0)))
                .disabled(!can_send)
                .tooltip(format!("{} ({submit_hint})", t(lang, "送信", "Send")))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.send_current_prompt(window, cx);
                    cx.notify();
                }))
        };

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
                    .flex_1()
                    .items_center()
                    .gap_1()
                    .min_w_0()
                    .overflow_hidden()
                    .child(attach_button)
                    .child(provider_button)
                    .child(model_button)
                    .children(effort_button)
                    .child(self.render_mode_menu(session, cx)),
            )
            .child(
                div()
                    .h_flex()
                    .flex_shrink_0()
                    .items_center()
                    .gap_2()
                    .when(running, |el| el.child(t(lang, "応答中…", "Responding…")))
                    .child(action_button),
            )
    }

    /// Chips for the skills and files attached to the next prompt, each removable.
    fn render_attachment_chips(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.attachments.is_empty() && self.selected_prompt_skills.is_empty() {
            return None;
        }
        let theme = cx.theme().clone();
        let lang = self.config.language;
        let skill_chips: Vec<AnyElement> = self
            .selected_prompt_skills
            .iter()
            .enumerate()
            .map(|(i, sk)| {
                let desc = sk.description.clone();
                div()
                    .id(SharedString::from(format!("prompt-skill-chip-{i}")))
                    .h_flex()
                    .items_center()
                    .gap_1()
                    .max_w(px(240.0))
                    .pl_2()
                    .pr_0p5()
                    .py_0p5()
                    .rounded_lg()
                    .border_1()
                    .border_color(accent().opacity(0.5))
                    .bg(accent().opacity(0.12))
                    .text_xs()
                    .text_color(accent_text())
                    .tooltip(move |window, cx| {
                        gpui_component::tooltip::Tooltip::new(desc.clone()).build(window, cx)
                    })
                    .child(div().min_w_0().truncate().child(format!("✦ ${}", sk.name)))
                    .child(
                        Button::new(SharedString::from(format!("prompt-skill-rm-{i}")))
                            .xsmall()
                            .ghost()
                            .icon(IconName::Close)
                            .rounded(ButtonRounded::Size(px(999.0)))
                            .tooltip(t(lang, "スキルを外す", "Remove skill"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if i < this.selected_prompt_skills.len() {
                                    this.selected_prompt_skills.remove(i);
                                }
                                cx.notify();
                            })),
                    )
                    .into_any_element()
            })
            .collect();
        Some(
            div()
                .h_flex()
                .flex_wrap()
                .gap_1p5()
                .children(skill_chips)
                .children(self.attachments.iter().enumerate().map(|(i, path)| {
                    let name = attachment_name(path);
                    let image = same_agent::adapters::is_image_path(&name);
                    let label = if image && self.pasted_attachments.contains(path) {
                        t(lang, "貼り付けた画像", "Pasted image").to_string()
                    } else {
                        name.clone()
                    };
                    let full = path.to_string_lossy().to_string();
                    let tip_path = path.clone();
                    let leading = if image {
                        // A thumbnail, so an attached image is recognizable
                        // without opening its (possibly generated) name.
                        div()
                            .size(px(30.0))
                            .flex_shrink_0()
                            .rounded_md()
                            .overflow_hidden()
                            .child(
                                gpui::img(path.clone())
                                    .size_full()
                                    .object_fit(ObjectFit::Cover),
                            )
                            .into_any_element()
                    } else {
                        Icon::new(IconName::File)
                            .xsmall()
                            .text_color(theme.muted_foreground)
                            .into_any_element()
                    };
                    div()
                        .id(SharedString::from(format!("attachment-{i}")))
                        .h_flex()
                        .items_center()
                        .gap_1p5()
                        .max_w(px(240.0))
                        .pl_1()
                        .pr_0p5()
                        .py_0p5()
                        .rounded_lg()
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.muted)
                        .text_xs()
                        .tooltip(move |window, cx| {
                            if image {
                                let p = tip_path.clone();
                                let full = full.clone();
                                gpui_component::tooltip::Tooltip::element(move |_, _| {
                                    div()
                                        .v_flex()
                                        .gap_1()
                                        .max_w(px(360.0))
                                        .child(
                                            gpui::img(p.clone())
                                                .max_w(px(360.0))
                                                .max_h(px(260.0))
                                                .object_fit(ObjectFit::Contain),
                                        )
                                        .child(div().text_xs().child(full.clone()))
                                })
                                .build(window, cx)
                            } else {
                                gpui_component::tooltip::Tooltip::new(full.clone())
                                    .build(window, cx)
                            }
                        })
                        .child(leading)
                        .child(div().min_w_0().truncate().child(label))
                        .child(
                            Button::new(SharedString::from(format!("attachment-remove-{i}")))
                                .xsmall()
                                .ghost()
                                .icon(IconName::Close)
                                .rounded(ButtonRounded::Size(px(999.0)))
                                .tooltip(t(lang, "添付を外す", "Remove"))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if i < this.attachments.len() {
                                        let removed = this.attachments.remove(i);
                                        this.cleanup_pasted_attachment(&removed);
                                    }
                                    cx.notify();
                                })),
                        )
                }))
                .into_any_element(),
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
                .child(gpui::img(crate::brand::shark_egg()).size(px(112.0)))
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

        let slash_cmds = if self.prompt_is_command {
            self.slash_commands
                .get(&session.id)
                .cloned()
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        let provider_issue = self
            .provider_checks
            .get(&session.provider_id)
            .and_then(|c| {
                let name = self.provider_name(&session.provider_id);
                if !c.available {
                    Some(format!(
                        "⚠ {} — {}",
                        match lang {
                            Language::Ja => format!("{name} を起動できません"),
                            Language::En => format!("{name} is not available"),
                        },
                        c.detail
                    ))
                } else if c.auth_status == ProviderAuthStatus::NotLoggedIn {
                    Some(format!(
                        "⚠ {}",
                        match lang {
                            Language::Ja => format!("{name} にログインしていません"),
                            Language::En => format!("{name} is not logged in"),
                        }
                    ))
                } else if c.auth_status == ProviderAuthStatus::NoSubscription {
                    Some(format!(
                        "⚠ {}",
                        match lang {
                            Language::Ja =>
                                format!("{name} の有効なサブスクリプションがありません"),
                            Language::En => format!("{name} has no active subscription"),
                        }
                    ))
                } else {
                    None
                }
            });

        let suggestions = self
            .render_skill_picker_popover(cx)
            .or_else(|| self.render_command_suggestions(&session, &slash_cmds, cx))
            .or_else(|| self.render_skill_suggestions(cx));
        let composer_bar = self.render_composer_bar(&session, running, cx);
        let attachment_chips = self.render_attachment_chips(cx);
        let paste_view = cx.entity();

        div()
            .v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(theme.background)
            .child(self.render_chat_header(&session, running, cx))
            .when_some(self.render_background_alerts_banner(cx), |el, banner| {
                el.child(banner)
            })
            .when_some(self.render_worktree_confirm_banner(cx), |el, banner| {
                el.child(banner)
            })
            .when_some(self.render_provider_switch_confirm(cx), |el, card| {
                el.child(card)
            })
            .when_some(provider_issue, |el, banner| {
                el.child(
                    div()
                        .h_flex()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .px_4()
                        .py_1()
                        .bg(theme.warning.opacity(0.12))
                        .child(
                            div()
                                .text_xs()
                                .text_color(status_amber())
                                .truncate()
                                .child(banner),
                        )
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
                let (tail_min_height, _) = self.sync_turn_anchor_tail_height();
                self.sync_feed();
                let is_scrolled_up = self.is_feed_scrolled_up();
                let has_turn_anchor = self.feed_turn_anchor.borrow().is_some();
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(
                        list(
                            self.feed.clone(),
                            cx.processor(move |this, ix: usize, _window, cx| {
                                this.render_feed_item(ix, tail_min_height, cx)
                            }),
                        )
                        .size_full(),
                    )
                    // Always visible (the default only shows while
                    // scrolling), so the position in a long chat is clear.
                    .child(
                        div().absolute().inset_0().child(
                            Scrollbar::new(&self.feed)
                                .id("chat-feed-scrollbar")
                                .axis(ScrollbarAxis::Vertical)
                                .mode(ScrollbarMode::Always)
                                .viewport_from_layout(),
                        ),
                    )
                    .when(is_scrolled_up, |el| {
                        el.child(
                            div()
                                .absolute()
                                .left_0()
                                .right_0()
                                .bottom_3()
                                .h_flex()
                                .justify_center()
                                .child(
                                    Button::new("btn-feed-jump-latest")
                                        .xsmall()
                                        .secondary()
                                        .rounded(ButtonRounded::Size(px(999.0)))
                                        .icon(IconName::ArrowDown)
                                        .label(t(lang, "最新へ", "Jump to latest"))
                                        .shadow_md()
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.scroll_feed_to_end();
                                            cx.notify();
                                        })),
                                ),
                        )
                    })
                    .on_prepaint({
                        let view = cx.entity().downgrade();
                        move |_bounds, window, cx| {
                            let needs_reflow = view
                                .read_with(&*cx, |this, _| {
                                    let anchor_changed =
                                        has_turn_anchor && this.sync_turn_anchor_tail_height().1;
                                    let jump_changed = this.is_feed_scrolled_up() != is_scrolled_up;
                                    anchor_changed || jump_changed
                                })
                                .unwrap_or(false);
                            if needs_reflow {
                                let view_id = view.entity_id();
                                window.on_next_frame(move |_, cx| cx.notify(view_id));
                            }
                        }
                    })
            })
            // Prompt composer: one rounded card holding attachments, the
            // text and a toolbar (like ChatGPT / T3 Code / OpenCode).
            .child(
                div().v_flex().px_4().pt_1().pb_4().child(
                    div()
                        .v_flex()
                        .w_full()
                        .max_w(px(920.0))
                        .mx_auto()
                        .gap_1()
                        .when_some(suggestions, |el, s| el.child(s))
                        .child(
                            div()
                                .id("composer-card")
                                .v_flex()
                                .w_full()
                                .gap_1()
                                .px_3()
                                .pt_2()
                                .pb_2()
                                .rounded(px(20.0))
                                .border_1()
                                .border_color(theme.border)
                                .bg(theme.secondary)
                                .shadow_md()
                                .drag_over::<ExternalPaths>(|style, _, _, cx| {
                                    style.border_color(cx.theme().ring).bg(cx.theme().accent)
                                })
                                .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                                    this.add_attachments(paths.paths().iter().cloned());
                                    this.prompt_input.update(cx, |ta, cx| ta.focus(window, cx));
                                    cx.notify();
                                }))
                                .when_some(attachment_chips, |el, chips| el.child(chips))
                                .child(
                                    div()
                                        .w_full()
                                        .min_w_0()
                                        // Up / Down recall earlier prompts.
                                        .capture_action(cx.listener(
                                            |this, _: &MoveUp, window, cx| {
                                                if this.recall_prompt_history(true, window, cx) {
                                                    cx.stop_propagation();
                                                }
                                            },
                                        ))
                                        .capture_action(cx.listener(
                                            |this, _: &MoveDown, window, cx| {
                                                if this.recall_prompt_history(false, window, cx) {
                                                    cx.stop_propagation();
                                                }
                                            },
                                        ))
                                        .child(
                                            Textarea::new(&self.prompt_input)
                                                .appearance(false)
                                                .on_paste(move |item, _window, cx| {
                                                    paste_view.update(cx, |this, cx| {
                                                        let consumed = this.handle_paste(item);
                                                        cx.notify();
                                                        consumed
                                                    })
                                                }),
                                        ),
                                )
                                .child(composer_bar),
                        ),
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
    fn render_feed_item(
        &mut self,
        ix: usize,
        tail_min_height: Pixels,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if ix < self.messages.len() {
            let cached_md = self.ensure_session_markdown_state_for_stored_message(ix, cx);
            let el = self.render_message(ix, &self.messages[ix], None, cached_md.as_ref(), cx);
            return self.feed_column(ix, el);
        }
        let Some(session) = self.current_session().cloned() else {
            return div().into_any_element();
        };
        let theme = cx.theme().clone();
        let lang = self.config.language;
        let running = self.is_session_running(&session.id);
        let live_msg = self.live_drafts.get(&session.id).map(|draft| {
            (
                Message {
                    id: draft.id.clone(),
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
                },
                draft.plan.clone(),
            )
        });
        let has_live = live_msg.is_some();
        let pending_perms: Vec<PermissionRequest> = self
            .pending_permissions
            .iter()
            .filter(|r| r.session_id == session.id)
            .cloned()
            .collect();
        let input_ixs: Vec<usize> = self
            .pending_inputs
            .iter()
            .enumerate()
            .filter(|(_, p)| p.request.session_id == session.id)
            .map(|(i, _)| i)
            .collect();
        let waiting = !pending_perms.is_empty() || !input_ixs.is_empty();

        let tail = div()
            .v_flex()
            .w_full()
            .gap_4()
            .when(self.messages.is_empty() && !has_live, |el| {
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
                        .child(gpui::img(crate::brand::shark_egg()).size(px(96.0)))
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
            .when_some(live_msg, |el, (m, plan)| {
                el.child(self.render_message(ix, &m, Some(waiting), None, cx))
                    .when_some(plan, |el, plan| {
                        el.child(self.render_plan_card(&m.id, &plan, false, cx))
                    })
            })
            // Only the first card takes keyboard answers.
            .children(
                pending_perms
                    .into_iter()
                    .enumerate()
                    .map(|(i, req)| self.render_permission(req, i == 0, cx)),
            )
            .children(input_ixs.iter().enumerate().map(|(n, &i)| {
                let first = n == 0 && self.pending_permissions.iter().all(|p| p.session_id != session.id);
                self.render_user_input(&self.pending_inputs[i], first, cx)
            }))
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
        let tail_el = if has_live {
            let live_height = self.feed_live_content_height.clone();
            tail.on_prepaint(move |bounds, _, _| {
                live_height.set(bounds.size.height);
            })
            .into_any_element()
        } else {
            tail.into_any_element()
        };
        let outer = div()
            .v_flex()
            .w_full()
            .when(tail_min_height > px(0.0), |el| el.min_h(tail_min_height))
            .child(tail_el);
        self.feed_column(ix, outer)
    }

    /// Renders one message. `live` is `Some(waiting_for_permission)` for the
    /// in-progress assistant reply.
    fn render_message(
        &self,
        ix: usize,
        msg: &Message,
        live: Option<bool>,
        cached_markdown: Option<&Entity<TextViewState>>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let lang = self.config.language;
        let is_user = msg.role == MessageRole::User;

        if is_user {
            let copy_text = split_error(&msg.content).0.to_string();
            let text_view = if let Some(state) = cached_markdown {
                TextView::new(state)
            } else {
                TextView::markdown(
                    SharedString::from(format!("md-{}", msg.id)),
                    user_markdown(&msg.content),
                )
            };
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
                                .child(text_view.selectable(true)),
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
        let (raw_body, error) = split_error(&msg.content);
        let (body, plan) = split_plan(raw_body);
        let plan = plan.map(str::to_string);
        let error = error.map(str::to_string);
        // Only the session's latest reply offers to implement its plan.
        let plan_actionable = !is_live
            && self.messages.last().is_some_and(|m| m.id == msg.id)
            && !self.is_session_running(&msg.session_id);

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
                        let copy_text = raw_body.to_string();
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
                msg.thinking.as_deref().filter(|s| !s.trim().is_empty()),
                |el, thinking| {
                    let key = think_key.clone();
                    let thinking_now = is_live && body.is_empty() && msg.tool_calls.is_empty();
                    let thinking_owned = if show_thinking {
                        Some(thinking.to_string())
                    } else {
                        None
                    };
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
                                        if !this.expanded_thinking.remove(&key) {
                                            this.expanded_thinking.insert(key.clone());
                                        }
                                        this.remeasure_feed_item(ix);
                                        cx.notify();
                                    })),
                            )
                            .when_some(thinking_owned, |el, thinking| {
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
            // Tool calls (compact 1-line summary; full details live in the right-pane Tools view)
            .when(!msg.tool_calls.is_empty(), |el| {
                el.child(self.render_tool_calls_summary(msg, cx))
            })
            // Message content
            .when(!body.trim().is_empty(), |el| {
                let text_view = if let Some(state) = cached_markdown {
                    TextView::new(state)
                } else if let Some(state) = self.tracked_markdown_state(msg, is_live) {
                    TextView::new(state)
                } else {
                    TextView::markdown(
                        SharedString::from(format!("md-{}", msg.id)),
                        body.to_string(),
                    )
                };
                el.child(div().text_sm().child(text_view.selectable(true)))
            })
            .when_some(plan, |el, plan| {
                el.child(self.render_plan_card(&msg.id, &plan, plan_actionable, cx))
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
                        .child(crate::brand::shark_egg_loader(
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

    fn select_right_tab(&mut self, top: bool, target: RightTab, _cx: &mut Context<Self>) {
        if self.right_split {
            if top {
                if self.right_bottom_tab == target {
                    self.right_bottom_tab = self.right_tab;
                }
                self.right_tab = target;
            } else {
                if self.right_tab == target {
                    self.right_tab = self.right_bottom_tab;
                }
                self.right_bottom_tab = target;
            }
        } else {
            self.right_tab = target;
        }
        if target == RightTab::Git {
            self.reload_git_and_worktrees_async(true);
        }
    }

    fn toggle_right_split(&mut self) {
        self.right_split = !self.right_split;
        if self.right_split && self.right_tab == self.right_bottom_tab {
            self.right_bottom_tab = if self.right_tab == RightTab::Tools {
                RightTab::Files
            } else {
                RightTab::Tools
            };
        }
    }

    fn open_tools_for_turn(&mut self, msg_id: String) {
        self.show_right_panel = true;
        self.selected_tool_turn_id = Some(msg_id);
        if self.right_split {
            if self.right_tab != RightTab::Tools && self.right_bottom_tab != RightTab::Tools {
                self.right_bottom_tab = RightTab::Tools;
            }
        } else {
            self.right_tab = RightTab::Tools;
        }
    }

    fn session_tool_call_count(&self) -> usize {
        let persisted: usize = self.messages.iter().map(|m| m.tool_calls.len()).sum();
        let live: usize = self
            .selected_session_id
            .as_deref()
            .and_then(|sid| self.live_drafts.get(sid))
            .map(|d| d.tool_calls.len())
            .unwrap_or(0);
        persisted + live
    }

    fn render_tool_calls_summary(&self, msg: &Message, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let lang = self.config.language;
        let (badge, color, summary) = summarize_tool_calls(&msg.tool_calls, lang);
        let selected = self.show_right_panel
            && (self.right_tab == RightTab::Tools
                || (self.right_split && self.right_bottom_tab == RightTab::Tools))
            && self.selected_tool_turn_id.as_deref() == Some(msg.id.as_str());
        let msg_id = msg.id.clone();

        div()
            .id(SharedString::from(format!("tc-summary-{}", msg.id)))
            .h_flex()
            .gap_2()
            .items_center()
            .px_2()
            .py_1()
            .rounded(px(6.0))
            .border_1()
            .border_color(if selected {
                Hsla::from(accent())
            } else {
                theme.border
            })
            .bg(if selected {
                theme.list_active
            } else {
                theme.sidebar
            })
            .text_xs()
            .cursor_pointer()
            .hover(|s| s.bg(theme.list_hover))
            .child(div().text_color(color).child(badge))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(theme.foreground)
                    .child(summary),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_color(theme.muted_foreground)
                    .child(t(lang, "詳細 →", "Details →")),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.open_tools_for_turn(msg_id.clone());
                cx.notify();
            }))
            .into_any_element()
    }

    fn render_tool_call(
        &self,
        msg_id: &str,
        i: usize,
        tc: &ToolCallRecord,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let lang = self.config.language;
        let tc_key = format!("{msg_id}-{}-{i}", tc.id);
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
                        if !this.expanded_tools.remove(&key_click) {
                            this.expanded_tools.insert(key_click.clone());
                        }
                        cx.notify();
                    })),
            )
            .when(expanded, |el| {
                let section = |label: &str, text: &str, id: String| {
                    let mut chars = text.chars();
                    let mut preview: String =
                        chars.by_ref().take(TOOL_OUTPUT_PREVIEW_CHARS).collect();
                    if chars.next().is_some() {
                        let remaining = 1 + chars.count();
                        preview.push_str(&match lang {
                            Language::Ja => format!("\n… (残り {remaining} 文字を省略)"),
                            Language::En => format!("\n… ({remaining} more chars truncated)"),
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
                        .when_some(tc.input.as_deref(), |el, inp| {
                            el.child(section(
                                t(lang, "入力", "Input"),
                                inp,
                                format!("tci-{tc_key}"),
                            ))
                        })
                        .when_some(tc.output.as_deref(), |el, out| {
                            el.child(section(
                                t(lang, "出力", "Output"),
                                out,
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

    fn render_right_tab_bar(
        &self,
        top: bool,
        active_tab: RightTab,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let lang = self.config.language;
        let prefix = if top { "top" } else { "bottom" };
        let tool_count = self.session_tool_call_count();
        let tab = |suffix: &'static str, label: String, active: bool| {
            Button::new(SharedString::from(format!("tab-{prefix}-{suffix}")))
                .xsmall()
                .when(active, |b| b.primary())
                .when(!active, |b| b.ghost())
                .label(label)
        };

        div()
            .h_flex()
            .justify_between()
            .items_center()
            .gap_1()
            .child(
                div()
                    .h_flex()
                    .gap_1()
                    .flex_wrap()
                    .child(
                        tab(
                            "files",
                            t(lang, "ファイル", "Files").into(),
                            active_tab == RightTab::Files,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.select_right_tab(top, RightTab::Files, cx);
                            cx.notify();
                        })),
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
                                "git",
                                if changes > 0 {
                                    format!("Git ({changes})")
                                } else {
                                    "Git".to_string()
                                },
                                active_tab == RightTab::Git,
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.select_right_tab(top, RightTab::Git, cx);
                                    cx.notify();
                                },
                            )),
                        )
                    })
                    .child(
                        tab(
                            "tools",
                            if tool_count > 0 {
                                match lang {
                                    Language::Ja => format!("ツール ({tool_count})"),
                                    Language::En => format!("Tools ({tool_count})"),
                                }
                            } else {
                                t(lang, "ツール", "Tools").into()
                            },
                            active_tab == RightTab::Tools,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.select_right_tab(top, RightTab::Tools, cx);
                            cx.notify();
                        })),
                    )
                    .child(
                        tab(
                            "logs",
                            t(lang, "ログ", "Logs").into(),
                            active_tab == RightTab::Logs,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.select_right_tab(top, RightTab::Logs, cx);
                            cx.notify();
                        })),
                    ),
            )
            .child(if top {
                div()
                    .h_flex()
                    .gap_1()
                    .flex_shrink_0()
                    .child(
                        Button::new("toggle-right-split")
                            .xsmall()
                            .when(self.right_split, |b| b.primary())
                            .when(!self.right_split, |b| b.ghost())
                            .tooltip(if self.right_split {
                                t(lang, "上下分割を閉じる", "Close vertical split")
                            } else {
                                t(lang, "右ペインを上下分割", "Split right pane vertically")
                            })
                            .label("⬓")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.toggle_right_split();
                                cx.notify();
                            })),
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
                    )
                    .into_any_element()
            } else {
                Button::new("close-right-split")
                    .xsmall()
                    .ghost()
                    .tooltip(t(lang, "分割を閉じる", "Close split"))
                    .label("×")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.right_split = false;
                        cx.notify();
                    }))
                    .into_any_element()
            })
            .into_any_element()
    }

    fn render_right_tab_content(&self, tab: RightTab, cx: &mut Context<Self>) -> AnyElement {
        match tab {
            RightTab::Files => self.render_files_tab(cx).into_any_element(),
            RightTab::Git => self.render_git_tab(cx).into_any_element(),
            RightTab::Tools => self.render_tools_tab(cx).into_any_element(),
            RightTab::Logs => self.render_logs_tab(cx).into_any_element(),
        }
    }

    fn render_tools_tab(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let lang = self.config.language;
        let selected_turn = self.selected_tool_turn_id.as_deref();

        // Collect all turns with tool calls in the active session (persisted + live),
        // numbering by actual assistant conversation turn before filtering or paging.
        let mut groups: Vec<(Option<usize>, String, usize, &[ToolCallRecord])> = self
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::Assistant)
            .enumerate()
            .filter(|(_, m)| !m.tool_calls.is_empty())
            .map(|(turn_ix, m)| {
                (
                    Some(turn_ix + 1),
                    m.id.clone(),
                    0usize,
                    m.tool_calls.as_slice(),
                )
            })
            .collect();
        if let Some(draft) = self
            .selected_session_id
            .as_deref()
            .and_then(|sid| self.live_drafts.get(sid))
            .filter(|d| !d.tool_calls.is_empty())
        {
            groups.push((None, draft.id.clone(), 0usize, draft.tool_calls.as_slice()));
        }

        let has_matching_filter =
            selected_turn.is_some_and(|tid| groups.iter().any(|(_, id, _, _)| id == tid));
        let mut hidden_tool_count = 0usize;
        if has_matching_filter {
            groups.retain(|(_, id, _, _)| Some(id.as_str()) == selected_turn);
        } else {
            // Show newest turns first so live/recent activity is immediately visible
            // at the top of the scroll area without fighting manual history browsing.
            let total_tools: usize = groups.iter().map(|(_, _, _, tcs)| tcs.len()).sum();
            let limit = self.tools_visible_limit.max(TOOLS_HISTORY_PAGE_SIZE);
            if total_tools > limit {
                hidden_tool_count = total_tools - limit;
                let mut remaining = limit;
                let mut paged = Vec::new();
                for (turn_no, msg_id, _, tcs) in groups.into_iter().rev() {
                    if remaining == 0 {
                        break;
                    }
                    if tcs.len() <= remaining {
                        remaining -= tcs.len();
                        paged.push((turn_no, msg_id, 0usize, tcs));
                    } else {
                        let start = tcs.len() - remaining;
                        paged.push((turn_no, msg_id, start, &tcs[start..]));
                        remaining = 0;
                    }
                }
                groups = paged;
            } else {
                groups.reverse();
            }
        }

        let scroll_id = SharedString::from(format!(
            "right-tools-scroll-{}-{}",
            self.selected_session_id.as_deref().unwrap_or("none"),
            if has_matching_filter {
                selected_turn.unwrap_or("all")
            } else {
                "all"
            }
        ));

        div()
            .v_flex()
            .flex_1()
            .min_h_0()
            .gap_2()
            .when(has_matching_filter, |el| {
                el.child(
                    div()
                        .h_flex()
                        .justify_between()
                        .items_center()
                        .px_2()
                        .py_1()
                        .rounded(px(6.0))
                        .bg(theme.background)
                        .border_1()
                        .border_color(theme.border)
                        .child(div().text_xs().text_color(theme.muted_foreground).child(t(
                            lang,
                            "選択中のターンのツール実行を表示中",
                            "Showing tool calls for selected turn",
                        )))
                        .child(
                            Button::new("clear-tool-turn-filter")
                                .xsmall()
                                .ghost()
                                .label(t(lang, "すべて表示", "Show all"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.selected_tool_turn_id = None;
                                    cx.notify();
                                })),
                        ),
                )
            })
            .child(
                div()
                    .id(scroll_id)
                    .v_flex()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .gap_2()
                    .when(groups.is_empty(), |el| {
                        el.child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .p_2()
                                .child(t(
                                    lang,
                                    "このセッションではまだツール実行がありません",
                                    "No tool calls in this session yet",
                                )),
                        )
                    })
                    .children(
                        groups
                            .into_iter()
                            .map(|(turn_no, msg_id, index_offset, tcs)| {
                                let total_in_turn = index_offset + tcs.len();
                                let header = match turn_no {
                                    None => t(lang, "実行中のターン", "Current turn").to_string(),
                                    Some(n) => match lang {
                                        Language::Ja => format!("ターン #{n} ({total_in_turn}件)"),
                                        Language::En => format!("Turn #{n} ({total_in_turn})"),
                                    },
                                };
                                let rows = if has_matching_filter {
                                    tcs.iter()
                                        .enumerate()
                                        .map(|(i, tc)| {
                                            self.render_tool_call(&msg_id, index_offset + i, tc, cx)
                                        })
                                        .collect::<Vec<_>>()
                                } else {
                                    tcs.iter()
                                        .enumerate()
                                        .rev()
                                        .map(|(i, tc)| {
                                            self.render_tool_call(&msg_id, index_offset + i, tc, cx)
                                        })
                                        .collect::<Vec<_>>()
                                };
                                div()
                                    .v_flex()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_xs()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(theme.muted_foreground)
                                            .child(header),
                                    )
                                    .children(rows)
                            }),
                    )
                    .when(hidden_tool_count > 0, |el| {
                        el.child(
                            div().h_flex().justify_center().child(
                                Button::new("load-earlier-tools")
                                    .xsmall()
                                    .ghost()
                                    .label(match lang {
                                        Language::Ja => format!(
                                            "以前のツール実行を表示 (残り {hidden_tool_count} 件)"
                                        ),
                                        Language::En => format!(
                                            "Show earlier tool calls ({hidden_tool_count} hidden)"
                                        ),
                                    })
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.tools_visible_limit = this
                                            .tools_visible_limit
                                            .saturating_add(TOOLS_HISTORY_PAGE_SIZE);
                                        cx.notify();
                                    })),
                            ),
                        )
                    }),
            )
    }

    fn render_right_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();

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
            .child(
                div()
                    .v_flex()
                    .flex_1()
                    .min_h_0()
                    .gap_2()
                    .child(self.render_right_tab_bar(true, self.right_tab, cx))
                    .child(self.render_right_tab_content(self.right_tab, cx)),
            )
            .when(self.right_split, |el| {
                el.child(div().h(px(1.0)).bg(theme.border)).child(
                    div()
                        .v_flex()
                        .flex_1()
                        .min_h_0()
                        .gap_2()
                        .child(self.render_right_tab_bar(false, self.right_bottom_tab, cx))
                        .child(self.render_right_tab_content(self.right_bottom_tab, cx)),
                )
            })
    }

    fn render_files_tab(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let lang = self.config.language;
        let filtering = !self.file_filter_input.read(cx).value().trim().is_empty();
        let file_open = self.opened_file_path.is_some();
        let root_workdir = self.current_workspace_root();

        div()
            .v_flex()
            .flex_1()
            .min_h_0()
            .gap_2()
            .child(Input::new(&self.file_filter_input).xsmall().cleanable(true))
            // File tree
            .child({
                let tree = div()
                    .id("files-tree-scroll")
                    .v_flex()
                    .when(file_open && !self.right_split, |d| {
                        d.h(px(220.0)).flex_shrink_0()
                    })
                    .when(file_open && self.right_split, |d| {
                        d.flex_1().min_h(px(56.0)).max_h(px(120.0))
                    })
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
                    .children(self.file_entries.iter().map(|entry| {
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
                        let menu_path = entry.path.clone();
                        let row_id = {
                            use std::hash::{Hash as _, Hasher as _};
                            let mut hasher = std::collections::hash_map::DefaultHasher::new();
                            entry.path.hash(&mut hasher);
                            SharedString::from(format!("file-entry-{:016x}", hasher.finish()))
                        };
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
                        list_row(row_id, selected, &theme)
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
                            .on_click(cx.listener(move |this, _, window, cx| {
                                if is_dir {
                                    if !this.expanded_dirs.remove(&path) {
                                        this.expanded_dirs.insert(path.clone());
                                    }
                                    this.reload_workspace_files_only(cx);
                                } else {
                                    this.open_file(path.clone(), window, cx);
                                }
                                cx.notify();
                            }))
                            .context_menu(move |menu, _, _| {
                                let reveal_path = menu_path.clone();
                                let copy_path = menu_path.clone();
                                menu.item(
                                    PopupMenuItem::new(reveal_in_file_manager_label(lang, is_dir))
                                        .on_click(move |_, _, _| {
                                            reveal_in_file_manager(&reveal_path, is_dir);
                                        }),
                                )
                                .item(
                                    PopupMenuItem::new(t(lang, "パスをコピー", "Copy Path"))
                                        .on_click(move |_, _, cx| {
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                copy_path.to_string_lossy().to_string(),
                                            ));
                                        }),
                                )
                            })
                    }));
                if let Some(workdir) = root_workdir {
                    tree.context_menu(move |menu, _, _| {
                        let workdir = workdir.clone();
                        menu.item(
                            PopupMenuItem::new(reveal_in_file_manager_label(lang, true)).on_click(
                                move |_, _, _| {
                                    open_path_in_file_manager(&workdir);
                                },
                            ),
                        )
                    })
                    .into_any_element()
                } else {
                    tree.into_any_element()
                }
            })
            // File viewer / editor when a file is open
            .when_some(self.opened_file_path.clone(), |el, path| {
                let rel = self
                    .current_project()
                    .and_then(|p| path.strip_prefix(&p.workdir).ok())
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .to_string();
                let ref_path = path.clone();
                let header_menu_path = path.clone();
                el.child(div().h(px(1.0)).bg(theme.border))
                    .child(
                        div()
                            .h_flex()
                            .justify_between()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .id("opened-file-header")
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_xs()
                                    .font_weight(FontWeight::BOLD)
                                    .child(if self.file_dirty {
                                        format!("● {rel}")
                                    } else {
                                        rel
                                    })
                                    .context_menu(move |menu, _, _| {
                                        let reveal_path = header_menu_path.clone();
                                        let copy_path = header_menu_path.clone();
                                        menu.item(
                                            PopupMenuItem::new(reveal_in_file_manager_label(
                                                lang, false,
                                            ))
                                            .on_click(move |_, _, _| {
                                                reveal_in_file_manager(&reveal_path, false);
                                            }),
                                        )
                                        .item(
                                            PopupMenuItem::new(t(
                                                lang,
                                                "パスをコピー",
                                                "Copy Path",
                                            ))
                                            .on_click(move |_, _, cx| {
                                                cx.write_to_clipboard(ClipboardItem::new_string(
                                                    copy_path.to_string_lossy().to_string(),
                                                ));
                                            }),
                                        )
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

fn reveal_in_file_manager_label(lang: Language, is_dir: bool) -> &'static str {
    #[cfg(target_os = "macos")]
    {
        if is_dir {
            t(lang, "Finder で開く", "Open in Finder")
        } else {
            t(lang, "Finder で表示", "Reveal in Finder")
        }
    }
    #[cfg(target_os = "windows")]
    {
        if is_dir {
            t(lang, "エクスプローラーで開く", "Open in File Explorer")
        } else {
            t(lang, "エクスプローラーで表示", "Reveal in File Explorer")
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if is_dir {
            t(lang, "ファイルマネージャで開く", "Open in File Manager")
        } else {
            t(lang, "親フォルダを開く", "Open Containing Folder")
        }
    }
}

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
            MainView::Skills => {
                crumbs.push(t(lang, "スキル", "Skills").into());
            }
            MainView::Usage => {
                crumbs.push(t(lang, "使用量", "Usage").into());
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
                            .child(gpui::img(crate::brand::shark_egg()).size(px(22.0)))
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
                    .when_some(self.render_quota_rings(cx), |el, rings| {
                        el.child(rings)
                            .child(div().w(px(1.0)).h(px(16.0)).mx_1().bg(theme.border))
                    })
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
                    })
                    // Keep its place elsewhere so the icons left of it don't move.
                    .when(self.active_view != MainView::Chat, |el| {
                        el.child(div().size(px(28.0)))
                    }),
            )
    }
}

impl Render for SameAppView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.reload_open_file_pending) {
            self.reload_open_file_from_disk(window, cx);
        }
        self.apply_pending_skill_draft(window, cx);
        self.ensure_input_fields(window, cx);
        // Take keyboard answers on a new card, unless the user is typing.
        if std::mem::take(&mut self.focus_approval) && self.active_view == MainView::Chat {
            let typing = !self.prompt_input.read(cx).value().is_empty()
                && self
                    .prompt_input
                    .read(cx)
                    .focus_handle(cx)
                    .is_focused(window);
            if !typing {
                window.focus(&self.approval_focus, cx);
            }
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
                if this.active_view == MainView::Skills && this.skills_ui.editor_dirty {
                    this.save_selected_skill(cx);
                    cx.notify();
                } else if this.opened_file_path.is_some() && this.file_dirty {
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
                    if this.show_skill_picker {
                        this.show_skill_picker = false;
                        this.prompt_input.update(cx, |ta, cx| ta.focus(window, cx));
                        cx.notify();
                    } else if this.renaming_session_id.take().is_some()
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
                        MainView::Skills => self.render_skills_view(cx).into_any_element(),
                        MainView::Usage => self.render_usage_view(cx).into_any_element(),
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

/// Extracts the first `https://` or `http://` URL from CLI login output.
fn extract_first_url(text: &str) -> Option<String> {
    for token in text.split_whitespace() {
        let cleaned = token.trim_matches(|c: char| {
            matches!(
                c,
                '(' | ')' | '[' | ']' | '<' | '>' | '"' | '\'' | ',' | ';'
            )
        });
        if cleaned.starts_with("https://") || cleaned.starts_with("http://") {
            return Some(cleaned.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_first_url_finds_oauth_link() {
        let out = "Open this link:\n\n  https://accounts.google.com/o/oauth2/auth?client_id=1&state=abc\n\nThen paste:";
        assert_eq!(
            extract_first_url(out).as_deref(),
            Some("https://accounts.google.com/o/oauth2/auth?client_id=1&state=abc")
        );
        assert_eq!(extract_first_url("no link here"), None);
    }

    #[test]
    fn strip_attachment_note_keeps_only_the_typed_text() {
        assert_eq!(strip_attachment_note("hello"), "hello");
        assert_eq!(strip_attachment_note("hi\n\n📎 `a.png`  📎 `b.txt`"), "hi");
        assert_eq!(strip_attachment_note("📎 `a.png`"), "");
        assert_eq!(strip_attachment_note("a\n\nb"), "a\n\nb");
    }

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

    #[test]
    fn live_draft_generates_unique_id_per_turn() {
        let first = LiveDraft::new();
        let second = LiveDraft::new();
        let third = LiveDraft::default();
        assert!(!first.id.is_empty());
        assert!(!second.id.is_empty());
        assert!(!third.id.is_empty());
        assert_ne!(first.id, second.id);
        assert_ne!(second.id, third.id);
        assert_ne!(first.id, third.id);
    }

    #[test]
    fn turn_tail_min_height_anchors_user_prompt_at_top_and_collapses_when_reply_fills_viewport() {
        assert_eq!(
            compute_turn_tail_min_height(px(600.0), px(74.0), None),
            px(494.0)
        );
        assert_eq!(
            compute_turn_tail_min_height(px(600.0), px(74.0), Some(px(200.0))),
            px(294.0)
        );
        assert_eq!(
            compute_turn_tail_min_height(px(600.0), px(74.0), Some(px(700.0))),
            px(0.0)
        );
        assert_eq!(
            compute_turn_tail_min_height(px(0.0), px(74.0), None),
            px(0.0)
        );
    }

    #[test]
    fn estimate_user_row_height_scales_with_lines_and_caps_at_viewport_fraction() {
        let short = estimate_user_row_height("hello", px(800.0));
        assert_eq!(short, px(78.0));

        let multi = estimate_user_row_height("a\nb\nc\nd", px(800.0));
        assert!(multi > short);

        let huge = estimate_user_row_height(&"line\n".repeat(100), px(800.0));
        assert_eq!(huge, px(520.0));
    }

    #[test]
    fn list_state_splice_and_finalize_preserve_scroll_and_tail_follow() {
        let feed = ListState::new(5, ListAlignment::Top, px(1000.0));
        let old = feed.item_count();
        feed.splice(old - 1..old, 2);
        feed.set_follow_mode(FollowMode::Tail);
        feed.scroll_to_end();
        assert_eq!(feed.item_count(), 6);
        assert_eq!(feed.logical_scroll_top().item_ix, 6);
        assert!(feed.is_following_tail());

        let old = feed.item_count();
        feed.scroll_to(gpui::ListOffset {
            item_ix: old - 1,
            offset_in_item: px(120.0),
        });
        feed.splice(old..old, 1);
        feed.remeasure_items(old - 1..old);
        assert_eq!(feed.item_count(), 7);
        let offset = feed.logical_scroll_top();
        assert_eq!(offset.item_ix, old - 1);
        assert_eq!(offset.offset_in_item, px(120.0));
    }

    #[test]
    fn diff_markdown_body_tracks_appends_unchanged_and_marker_truncation() {
        let initial = "hello";
        let synced_len = message_markdown_body(initial).len();

        let appended = "hello world";
        assert_eq!(
            diff_markdown_body(message_markdown_body(appended), synced_len),
            MarkdownBodySync::Append(" world")
        );

        let error_appended = format!("hello world{ERROR_MARKER}tail error");
        let after_marker_len = message_markdown_body(&error_appended).len();
        let error_more = format!("{error_appended} more");
        assert_eq!(
            diff_markdown_body(message_markdown_body(&error_more), after_marker_len),
            MarkdownBodySync::Unchanged
        );

        let partial = "hello\n\n<!--same:error--";
        let partial_len = message_markdown_body(partial).len();
        let completed = "hello\n\n<!--same:error-->boom";
        assert_eq!(
            diff_markdown_body(message_markdown_body(completed), partial_len),
            MarkdownBodySync::Replace("hello")
        );

        let plan_appended = format!("hello{PLAN_MARKER}step 1");
        let after_plan_len = message_markdown_body(&plan_appended).len();
        let plan_more = format!("{plan_appended}\nstep 2");
        assert_eq!(
            diff_markdown_body(message_markdown_body(&plan_more), after_plan_len),
            MarkdownBodySync::Unchanged
        );
    }

    #[test]
    fn live_draft_incremental_markdown_body_matches_full_split() {
        let spaces_24 = " ".repeat(24);
        let long_ws_case =
            format!("hello\n\n<!--same:plan-->\n{spaces_24}\n\n<!--same:error-->boom");
        let cases = [
            "こんにちは、日本語テキストと ASCII markdown `code` の混在応答です。",
            "末尾空白あり   \n\n<!--same:error-->connection dropped: 詳細はログ参照",
            "計画前の日本語本文  \n\n<!--same:plan-->\n日本語プラン手順１：修正する\n手順２：検証する",
            "hello\n\n<!--same:plan-->\n\n<!--same:error-->boom",
            "hello\n\n<!--same:plan-->\n   \n\n<!--same:error-->boom",
            long_ws_case.as_str(),
            "hello\n\n<!--same:plan-->\n\u{3000}\n\n<!--same:error-->boom",
            "hello\n\n<!--same:plan-->\n\u{3000}次のテキスト",
            "hello\n\n<!--same:plan-->\n  \n\n<!--same:error--X\n\n<!--same:error-->boom",
            "日本語テスト\n\n<!--same:plan-->\nステップ日本語\n\n<!--same:error-->失敗詳細",
        ];

        for full in cases {
            let mut single_char = LiveDraft::new();
            for ch in full.chars() {
                let mut buf = [0u8; 4];
                single_char.push_content(ch.encode_utf8(&mut buf));
                assert_eq!(
                    single_char.markdown_body(),
                    message_markdown_body(&single_char.content),
                    "single-char mismatch on prefix {:?}",
                    single_char.content
                );
            }

            let boundaries: Vec<usize> = full.char_indices().map(|(i, _)| i).collect();
            for &split_idx in &boundaries {
                let mut two_chunks = LiveDraft::new();
                two_chunks.push_content(&full[..split_idx]);
                assert_eq!(
                    two_chunks.markdown_body(),
                    message_markdown_body(&two_chunks.content)
                );
                two_chunks.push_content(&full[split_idx..]);
                assert_eq!(
                    two_chunks.markdown_body(),
                    message_markdown_body(&two_chunks.content)
                );
            }
        }

        let mut split_ws = LiveDraft::new();
        for chunk in [
            "hello\n\n<!--same:plan-->\n",
            spaces_24.as_str(),
            "\n\n<!--same:error-->boom",
        ] {
            split_ws.push_content(chunk);
            assert_eq!(
                split_ws.markdown_body(),
                message_markdown_body(&split_ws.content)
            );
        }
    }

    #[test]
    fn summarize_tool_calls_reflects_running_failed_and_completed_states() {
        let mk = |id: &str, title: &str, status: ToolCallStatus| ToolCallRecord {
            id: id.into(),
            title: title.into(),
            kind: Some("command".into()),
            status,
            input: None,
            output: None,
        };

        let running = vec![
            mk("1", "git status", ToolCallStatus::Completed),
            mk("2", "cargo test\n--workspace", ToolCallStatus::InProgress),
        ];
        let (badge, _, label_ja) = summarize_tool_calls(&running, Language::Ja);
        assert_eq!(badge, "◐");
        assert_eq!(label_ja, "ツール実行中 (1/2件): cargo test --workspace");

        let waiting = vec![
            mk("1", "git status", ToolCallStatus::Completed),
            mk("2", "cargo clippy", ToolCallStatus::Pending),
        ];
        let (badge, _, label_en) = summarize_tool_calls(&waiting, Language::En);
        assert_eq!(badge, "○");
        assert_eq!(label_en, "Waiting 1/2: cargo clippy");

        let failed = vec![
            mk("1", "git status", ToolCallStatus::Completed),
            mk("2", "cargo test", ToolCallStatus::Failed),
        ];
        let (badge, _, label_en) = summarize_tool_calls(&failed, Language::En);
        assert_eq!(badge, "✗");
        assert_eq!(label_en, "Ran 2 tools (1 failed)");

        let denied_only = vec![mk("1", "rm -rf /tmp/x", ToolCallStatus::Denied)];
        let (badge, _, label_en) = summarize_tool_calls(&denied_only, Language::En);
        assert_eq!(badge, "⊘");
        assert_eq!(label_en, "1 tool denied");

        let done_one = vec![mk("1", "git status", ToolCallStatus::Completed)];
        let (badge, _, label_en) = summarize_tool_calls(&done_one, Language::En);
        assert_eq!(badge, "✓");
        assert_eq!(label_en, "Ran 1 tool");
    }

    #[test]
    fn list_state_uniform_height_and_readiness_stabilize_manual_scroll() {
        let feed = ListState::new(0, ListAlignment::Top, px(1000.0));
        feed.set_item_measurement_readiness(|_ix, _| true);
        feed.reset_with_uniform_height(200, FEED_UNMEASURED_ITEM_HEIGHT_HINT);
        assert_eq!(feed.item_count(), 200);
        // Unmeasured items carry the uniform height hint instead of 0px, so
        // total height is immediately non-zero without calling `measure_all()`.
        assert_eq!(
            feed.max_offset_for_scrollbar().y,
            FEED_UNMEASURED_ITEM_HEIGHT_HINT * 200.0
        );

        let anchor = gpui::ListOffset {
            item_ix: 120,
            offset_in_item: px(45.0),
        };
        feed.scroll_to(anchor);
        assert_eq!(feed.logical_scroll_top(), anchor);

        // Remeasuring an item (e.g. after async >4 KiB Markdown finishes)
        // preserves the manual scroll anchor.
        feed.remeasure_items(119..120);
        assert_eq!(feed.logical_scroll_top(), anchor);
    }
}
