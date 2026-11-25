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
            Self::Running | Self::WaitingForApproval | Self::WaitingForSubagent
        )
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlashCommandInfo {
    pub name: String,
    pub description: String,
}

/// A model an agent reported it can use (e.g. from an ACP session).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
}
