pub mod manager;
pub mod resolver;
pub mod validator;

pub use manager::{
    ImportCandidate, ImportConflictPolicy, ImportPreview, SkillManager, SkillManagerError,
};
pub use resolver::{
    resolve_skills, EffectiveSkillState, ResolvedSkillEntry, SkillConflict, SkillConflictKind,
    SkillResolution,
};
pub use validator::{
    compute_dir_hash, parse_skill_md, render_skill_md, validate_skill_dir, validate_skill_md,
    validate_skill_name, ParsedSkillMd, RelatedFileInfo, SkillValidationIssue,
    SkillValidationReport, ValidationSeverity,
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Management scope of a SAME-owned skill.
///
/// Ownership (`scope` + `scope_id` / `project_id`) is intentionally kept
/// separate from runtime applicability so future scopes (Workspace, Quest,
/// AgentProfile, Session) can be added without breaking existing data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillScope {
    Global,
    Project,
}

impl SkillScope {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Project => "project",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "project" => Self::Project,
            _ => Self::Global,
        }
    }
}

/// Who owns a skill discovered or managed by SAME.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillOrigin {
    Global,
    Project,
    External,
    BuiltIn,
}

impl SkillOrigin {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Project => "project",
            Self::External => "external",
            Self::BuiltIn => "built_in",
        }
    }
}

/// How a SAME-managed skill was created or acquired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillSourceType {
    Manual,
    Local,
    Git,
    External,
}

impl SkillSourceType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Local => "local",
            Self::Git => "git",
            Self::External => "external",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "local" => Self::Local,
            "git" => Self::Git,
            "external" => Self::External,
            _ => Self::Manual,
        }
    }
}

/// A skill managed by SAME (canonical files live on disk; metadata is cached in SQLite).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skill {
    pub id: String,
    pub name: String,
    pub description: String,
    pub scope: SkillScope,
    pub project_id: Option<String>,
    /// Canonical directory path on the host filesystem (contains `SKILL.md`).
    pub path: String,
    pub source_type: SkillSourceType,
    pub source_uri: Option<String>,
    pub source_ref: Option<String>,
    pub source_revision: Option<String>,
    pub enabled: bool,
    pub content_hash: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Per-project override for a Global skill's enabled state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillAssignment {
    pub skill_id: String,
    pub project_id: String,
    pub enabled: bool,
}

/// How SAME delivers skills to a specific provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillDeploymentMethod {
    ClaudePluginDir,
    CodexExtraRoots,
    OpenCodeConfigEnv,
    AntigravityAddDir,
    StagingCopy,
    #[default]
    Unsupported,
}

impl SkillDeploymentMethod {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ClaudePluginDir => "claude_plugin_dir",
            Self::CodexExtraRoots => "codex_extra_roots",
            Self::OpenCodeConfigEnv => "opencode_config_env",
            Self::AntigravityAddDir => "antigravity_add_dir",
            Self::StagingCopy => "staging_copy",
            Self::Unsupported => "unsupported",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "claude_plugin_dir" => Self::ClaudePluginDir,
            "codex_extra_roots" => Self::CodexExtraRoots,
            "opencode_config_env" => Self::OpenCodeConfigEnv,
            "antigravity_add_dir" => Self::AntigravityAddDir,
            "staging_copy" => Self::StagingCopy,
            _ => Self::Unsupported,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::ClaudePluginDir => "Claude --plugin-dir",
            Self::CodexExtraRoots => "Codex extraRoots",
            Self::OpenCodeConfigEnv => "OpenCode config env",
            Self::AntigravityAddDir => "Antigravity --add-dir",
            Self::StagingCopy => "Staging copy",
            Self::Unsupported => "Unsupported",
        }
    }
}

/// Sync / availability state of a skill for an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillDeploymentStatus {
    #[default]
    Available,
    Disabled,
    Syncing,
    Synced,
    Conflict,
    Unsupported,
    RestartRequired,
    Error,
}

impl SkillDeploymentStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Disabled => "disabled",
            Self::Syncing => "syncing",
            Self::Synced => "synced",
            Self::Conflict => "conflict",
            Self::Unsupported => "unsupported",
            Self::RestartRequired => "restart_required",
            Self::Error => "error",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "available" => Self::Available,
            "disabled" => Self::Disabled,
            "syncing" => Self::Syncing,
            "synced" => Self::Synced,
            "conflict" => Self::Conflict,
            "unsupported" => Self::Unsupported,
            "restart_required" => Self::RestartRequired,
            "error" => Self::Error,
            _ => Self::Available,
        }
    }
}

/// Recorded deployment state of a skill for a given provider + project + execution target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillDeployment {
    pub skill_id: String,
    pub provider_id: String,
    /// Project ID, or empty string `""` for non-project / global-only sessions.
    pub project_id: String,
    pub execution_target: String,
    pub deployment_path: String,
    pub deployment_method: SkillDeploymentMethod,
    pub deployed_hash: String,
    pub status: SkillDeploymentStatus,
    pub last_error: Option<String>,
    pub last_synced_at: Option<DateTime<Utc>>,
}

/// An external or built-in skill discovered from an agent's own directories or a project repo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalSkill {
    pub name: String,
    pub description: String,
    pub origin: SkillOrigin,
    /// Provider that owns or reads this location (e.g. "claude", "codex", "opencode", "antigravity", "shared").
    pub provider_id: String,
    /// Display label of the discovery source (e.g. "~/.claude/skills", ".agents/skills").
    pub source_label: String,
    /// Host path to the skill directory.
    pub path: String,
    pub execution_target: String,
    pub has_scripts: bool,
    pub related_files: Vec<RelatedFileInfo>,
}
