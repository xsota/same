use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub id: String,
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub workspace_id: String,
    pub name: String,
    pub description: String,
    pub workdir: String,
    pub instructions: String,
    pub pinned: bool,
    pub archived: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestStatus {
    Active,
    Paused,
    Completed,
    Archived,
}

impl QuestStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Completed => "completed",
            Self::Archived => "archived",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "paused" => Self::Paused,
            "completed" => Self::Completed,
            "archived" => Self::Archived,
            _ => Self::Active,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Quest {
    pub id: String,
    pub project_id: String,
    pub title: String,
    pub goal: String,
    pub instructions: String,
    pub status: QuestStatus,
    pub pinned: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Idle,
    Running,
    WaitingForApproval,
    WaitingForUserInput,
    WaitingForSubagent,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

impl SessionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running => "running",
            Self::WaitingForApproval => "waiting_for_approval",
            Self::WaitingForUserInput => "waiting_for_user_input",
            Self::WaitingForSubagent => "waiting_for_subagent",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "running" => Self::Running,
            "waiting_for_approval" => Self::WaitingForApproval,
            "waiting_for_user_input" => Self::WaitingForUserInput,
            "waiting_for_subagent" => Self::WaitingForSubagent,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            "interrupted" => Self::Interrupted,
            _ => Self::Idle,
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(
            self,
            Self::Running
                | Self::WaitingForApproval
                | Self::WaitingForUserInput
                | Self::WaitingForSubagent
        )
    }
}

/// How much an agent may do without asking, chosen per session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    /// Read and plan only; the agent proposes a plan to approve.
    Plan,
    /// Ask before edits and commands (each agent's own default).
    #[default]
    Ask,
    /// Edits in the workspace go through; other actions still ask.
    AcceptEdits,
    /// Nothing asks, and sandboxes are off.
    FullAccess,
}

impl ApprovalMode {
    pub const ALL: [ApprovalMode; 4] = [
        ApprovalMode::Plan,
        ApprovalMode::Ask,
        ApprovalMode::AcceptEdits,
        ApprovalMode::FullAccess,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Ask => "ask",
            Self::AcceptEdits => "accept_edits",
            Self::FullAccess => "full_access",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.as_str() == s)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub project_id: String,
    pub quest_id: Option<String>,
    pub title: String,
    pub provider_id: String,
    pub model: Option<String>,
    /// Reasoning effort (e.g. "high"); `None` leaves it to the agent.
    pub effort: Option<String>,
    /// `None` uses the configured default.
    #[serde(default)]
    pub approval_mode: Option<ApprovalMode>,
    /// Pending or per-session handoff strategy persisted across restarts.
    #[serde(default)]
    pub handoff_mode: Option<HandoffMode>,
    /// Message ID boundary set by an explicit Fresh handoff; messages up to
    /// and including this ID are never sent in subsequent handoffs or fallbacks.
    #[serde(default)]
    pub handoff_cutoff_message_id: Option<String>,
    pub external_session_id: Option<String>,
    pub status: SessionStatus,
    pub pinned: bool,
    pub archived: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Assistant,
    System,
}

impl MessageRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::System => "system",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "user" => Self::User,
            "system" => Self::System,
            _ => Self::Assistant,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
    Denied,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRecord {
    pub id: String,
    pub title: String,
    pub kind: Option<String>,
    pub status: ToolCallStatus,
    pub input: Option<String>,
    pub output: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub session_id: String,
    pub role: MessageRole,
    pub content: String,
    pub thinking: Option<String>,
    pub tool_calls: Vec<ToolCallRecord>,
    pub provider_id: Option<String>,
    pub model: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Acp,
    ClaudeCli,
    CodexCli,
}

impl ProviderKind {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Acp => "ACP",
            Self::ClaudeCli => "Claude CLI",
            Self::CodexCli => "Codex CLI",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub id: String,
    pub name: String,
    pub kind: ProviderKind,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub default_model: Option<String>,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub description: String,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderCapabilities {
    pub supports_acp: bool,
    pub supports_streaming: bool,
    pub supports_thinking: bool,
    pub supports_tool_calls: bool,
    pub supports_permissions: bool,
    pub supports_cancel: bool,
    pub supports_session_resume: bool,
    pub supports_model_select: bool,
    pub supports_slash_commands: bool,
    #[serde(default)]
    pub supports_read_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionOption {
    pub id: String,
    pub name: String,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRequest {
    pub request_id: String,
    pub session_id: String,
    pub tool_call_id: Option<String>,
    pub title: String,
    pub description: String,
    pub options: Vec<PermissionOption>,
    /// A unified diff of the change, when the tool edits files.
    #[serde(default)]
    pub diff: Option<String>,
}

/// An agent asking the user questions (Claude Code's AskUserQuestion, Codex
/// `request_user_input`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserInputRequest {
    pub request_id: String,
    pub session_id: String,
    pub questions: Vec<UserQuestion>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserQuestion {
    pub id: String,
    pub header: String,
    pub question: String,
    pub options: Vec<UserQuestionOption>,
    pub multi_select: bool,
    /// A free-text answer is accepted besides the options.
    pub allow_other: bool,
    /// The free-text answer should be masked.
    #[serde(default)]
    pub secret: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserQuestionOption {
    pub label: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlashCommandInfo {
    pub name: String,
    pub description: String,
}

/// A model an agent reported it can use (e.g. from an ACP session).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Reasoning effort levels this model accepts (e.g. "low", "high");
    /// empty when the agent reports none.
    #[serde(default)]
    pub efforts: Vec<String>,
    /// The effort the agent uses for this model when none is chosen.
    #[serde(default)]
    pub default_effort: Option<String>,
}

/// Which limit a [`QuotaWindow`] reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum QuotaWindowKind {
    /// A rolling limit over the given number of minutes (e.g. 300 = 5 hours,
    /// 10080 = 7 days).
    Rolling { minutes: u32 },
    /// A rolling limit that applies to one model family only (e.g. "Opus").
    Model { minutes: u32, model: String },
}

/// One usage limit of a provider's plan, as the provider reported it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaWindow {
    pub kind: QuotaWindowKind,
    /// Share of the limit already used, 0–100.
    pub used_percent: f64,
    /// When the window resets, if the provider said.
    pub resets_at: Option<DateTime<Utc>>,
}

/// A provider's usage and limits at one point in time. Only what the
/// provider reported; nothing is estimated.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaSnapshot {
    /// The plan of the logged-in account (e.g. "pro", "plus").
    pub plan: Option<String>,
    pub windows: Vec<QuotaWindow>,
    pub fetched_at: DateTime<Utc>,
}

/// Outcome of asking a provider for its quota.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum QuotaReport {
    Available(QuotaSnapshot),
    /// The provider cannot report its quota here (e.g. an ACP agent, API-key
    /// auth, or a CLI too old to answer). The text says why.
    Unavailable(String),
}

/// Token counts, split as the providers bill them. `output` includes
/// reasoning / thinking tokens.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenCounts {
    /// Input tokens not read from the prompt cache.
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl TokenCounts {
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write
    }

    pub fn add(&mut self, other: &TokenCounts) {
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
    }

    pub fn saturating_sub(&self, other: &TokenCounts) -> TokenCounts {
        TokenCounts {
            input: self.input.saturating_sub(other.input),
            output: self.output.saturating_sub(other.output),
            cache_read: self.cache_read.saturating_sub(other.cache_read),
            cache_write: self.cache_write.saturating_sub(other.cache_write),
        }
    }

    pub fn is_zero(&self) -> bool {
        self.total() == 0
    }
}

/// Tokens a provider used over the last days, from its local history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenStats {
    /// Every day of the period (local dates), oldest first.
    pub days: Vec<(chrono::NaiveDate, TokenCounts)>,
    /// Per model, most used first.
    pub models: Vec<(String, TokenCounts)>,
    pub total: TokenCounts,
    /// Cost in USD, when the provider itself reports one (never estimated).
    pub cost_usd: Option<f64>,
    pub fetched_at: DateTime<Utc>,
}

/// Outcome of reading a provider's token history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum StatsReport {
    Available(TokenStats),
    /// No usable history for this provider. The text says why.
    Unavailable(String),
}

/// Strategy for handing off conversation context when switching providers or
/// recovering from an unavailable native session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffMode {
    /// Structured summary + recent conversation + key tool results + environment.
    #[default]
    Smart,
    /// Conversation history within the model's context budget.
    Full,
    /// Start a fresh provider session without passing prior conversation context
    /// (SAME conversation history is preserved).
    Fresh,
}

impl HandoffMode {
    pub const ALL: [HandoffMode; 3] = [HandoffMode::Smart, HandoffMode::Full, HandoffMode::Fresh];

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Smart => "smart",
            Self::Full => "full",
            Self::Fresh => "fresh",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.as_str() == s)
    }
}

/// Working directory isolation mode for a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkdirMode {
    /// Share the project's working directory.
    #[default]
    Shared,
    /// Isolate edits in a dedicated Git worktree.
    Isolated,
    /// Read-only investigation/review mode (enforced via provider permissions/sandbox).
    ReadOnly,
}

impl WorkdirMode {
    pub const ALL: [WorkdirMode; 3] = [
        WorkdirMode::Shared,
        WorkdirMode::Isolated,
        WorkdirMode::ReadOnly,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Shared => "shared",
            Self::Isolated => "isolated",
            Self::ReadOnly => "read_only",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.as_str() == s)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderSessionStatus {
    #[default]
    Active,
    Invalidated,
    Archived,
}

impl ProviderSessionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Invalidated => "invalidated",
            Self::Archived => "archived",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "invalidated" => Self::Invalidated,
            "archived" => Self::Archived,
            _ => Self::Active,
        }
    }
}

/// A provider-native session associated with a SAME logical session.
/// One SAME session may hold multiple `ProviderSession` records across
/// different providers, execution environments, or working directories.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderSession {
    pub id: String,
    pub session_id: String,
    pub provider_id: String,
    pub external_session_id: String,
    pub model: Option<String>,
    /// Execution environment key (e.g. "auto", "host", "wsl").
    pub exec_mode: String,
    pub workdir: String,
    /// ID of the last SAME `Message` already known to this native session.
    pub last_synced_message_id: Option<String>,
    pub status: ProviderSessionStatus,
    pub created_at: DateTime<Utc>,
    pub last_used_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRunStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl AgentRunStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            _ => Self::Running,
        }
    }
}

/// One execution turn of a provider within a SAME session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRun {
    pub id: String,
    pub session_id: String,
    pub provider_session_id: Option<String>,
    pub provider_id: String,
    pub model: Option<String>,
    pub status: AgentRunStatus,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
}

/// Environment snapshot captured in structured handoff context.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextEnvironment {
    pub workdir: String,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub workdir_mode: WorkdirMode,
}

/// Provider-agnostic structured summary of a session's context.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructuredContext {
    pub objective: String,
    #[serde(default)]
    pub decisions: Vec<String>,
    #[serde(default)]
    pub constraints: Vec<String>,
    #[serde(default)]
    pub progress: Vec<String>,
    #[serde(default)]
    pub pending_tasks: Vec<String>,
    #[serde(default)]
    pub artifacts: Vec<String>,
    #[serde(default)]
    pub tool_results: Vec<String>,
    #[serde(default)]
    pub errors: Vec<String>,
    #[serde(default)]
    pub environment: ContextEnvironment,
}

/// Persisted checkpoint of compacted/structured context for a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextCheckpoint {
    pub id: String,
    pub session_id: String,
    pub summary: StructuredContext,
    pub covered_until_message_id: Option<String>,
    pub token_estimate: usize,
    pub created_at: DateTime<Utc>,
}

/// Working directory binding for a session (shared, isolated worktree, or read-only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionWorkdir {
    pub session_id: String,
    pub mode: WorkdirMode,
    pub path: String,
    pub base_branch: Option<String>,
    pub branch_name: Option<String>,
    pub is_worktree: bool,
    pub created_at: DateTime<Utc>,
}
