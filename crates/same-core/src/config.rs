use crate::models::{ApprovalMode, HandoffMode, ProviderConfig, ProviderKind, WorkdirMode};
use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeMode {
    Dark,
    Light,
}

impl Default for ThemeMode {
    fn default() -> Self {
        Self::Dark
    }
}

/// The colour theme. Each theme has a light and a dark palette; which one
/// is shown follows [`ThemeMode`]. Themes avoid green / amber / red as their
/// primary colour, since those mean status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColorTheme {
    /// Deep Ocean / Shark Blue / Sea Foam — the brand theme.
    #[default]
    Ocean,
    /// Neutral greys with a teal accent (the look before themes existed).
    Classic,
    Twilight,
    Sakura,
}

impl ColorTheme {
    pub const ALL: [ColorTheme; 4] = [
        ColorTheme::Ocean,
        ColorTheme::Classic,
        ColorTheme::Twilight,
        ColorTheme::Sakura,
    ];

    pub fn label(self) -> (&'static str, &'static str) {
        match self {
            ColorTheme::Ocean => ("オーシャン", "Ocean"),
            ColorTheme::Classic => ("クラシック", "Classic"),
            ColorTheme::Twilight => ("トワイライト", "Twilight"),
            ColorTheme::Sakura => ("サクラ", "Sakura"),
        }
    }
}

/// Which key sends the chat prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubmitKey {
    /// Enter sends; Shift+Enter inserts a newline.
    #[default]
    Enter,
    /// Ctrl+Enter (Cmd+Enter on macOS) sends; Enter inserts a newline.
    ModEnter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    Ja,
    En,
}

impl Default for Language {
    fn default() -> Self {
        Self::Ja
    }
}

/// Where a project's agents run. Only meaningful on Windows, where a project
/// can live inside a WSL distribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecMode {
    /// Inside WSL if the project folder is in WSL, else natively.
    #[default]
    Auto,
    /// Always native (Windows) binaries.
    Host,
    /// Always inside WSL.
    Wsl,
}

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub config_file: PathBuf,
    pub db_file: PathBuf,
}

impl AppPaths {
    pub fn resolve() -> Result<Self> {
        let proj = ProjectDirs::from("rip", "shark", "same")
            .context("Failed to determine OS config/data directories")?;
        let config_dir = proj.config_dir().to_path_buf();
        let data_dir = proj.data_dir().to_path_buf();
        fs::create_dir_all(&config_dir)?;
        fs::create_dir_all(&data_dir)?;
        Ok(Self {
            config_file: config_dir.join("config.toml"),
            db_file: data_dir.join("same.db"),
            config_dir,
            data_dir,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub theme: ThemeMode,
    /// Colour theme; `theme` picks its light or dark palette. (Older configs
    /// had an `accent` key instead, which is now ignored.)
    #[serde(default)]
    pub color_theme: ColorTheme,
    /// UI font family. `None` uses the platform default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui_font: Option<String>,
    /// Font family for code, logs and the file editor. `None` uses the
    /// platform default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mono_font: Option<String>,
    /// Base UI font size in px; every rem-based size scales with it.
    /// `None` uses the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_size: Option<f32>,
    #[serde(default)]
    pub language: Language,
    #[serde(default = "default_provider_id")]
    pub default_provider_id: String,
    /// Replaced by `default_approval_mode`; read only to migrate old configs.
    #[serde(default, skip_serializing)]
    pub auto_approve_tools: bool,
    /// Set by [`AppConfig::load_or_default`] when it migrated
    /// `auto_approve_tools`, so the app can tell the user once.
    #[serde(skip)]
    pub migrated_auto_approve: bool,
    /// The approval mode of sessions that have not picked one.
    #[serde(default)]
    pub default_approval_mode: ApprovalMode,
    /// OS notifications when an agent waits for the user or finishes while
    /// the window is in the background.
    #[serde(default = "default_true")]
    pub notifications: bool,
    #[serde(default)]
    pub submit_key: SubmitKey,
    #[serde(default = "default_providers")]
    pub providers: Vec<ProviderConfig>,
    /// Per-project overrides of [`ExecMode`], keyed by project folder.
    /// Projects not listed use [`ExecMode::Auto`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub project_exec: BTreeMap<String, ExecMode>,
    /// Optional maximum number of sessions that may run concurrently across all projects.
    /// `None` (the default) means unlimited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent_sessions: Option<usize>,
    /// Default context handoff strategy when switching providers.
    #[serde(default)]
    pub default_handoff_mode: HandoffMode,
    /// Default working directory isolation mode for newly created sessions.
    #[serde(default)]
    pub default_workdir_mode: WorkdirMode,
}

fn default_true() -> bool {
    true
}

pub const MIN_CONCURRENT_SESSIONS: usize = 1;
pub const MAX_CONCURRENT_SESSIONS: usize = 16;

fn default_provider_id() -> String {
    "opencode".to_string()
}

pub fn default_providers() -> Vec<ProviderConfig> {
    vec![
        ProviderConfig {
            id: "opencode".to_string(),
            name: "OpenCode (ACP)".to_string(),
            kind: ProviderKind::Acp,
            enabled: true,
            command: "opencode".to_string(),
            args: vec!["acp".to_string()],
            env: BTreeMap::new(),
            default_model: None,
            models: vec![],
            description: "OpenCode via Agent Client Protocol (opencode acp)".to_string(),
        },
        ProviderConfig {
            id: "claude".to_string(),
            name: "Claude Code".to_string(),
            kind: ProviderKind::ClaudeCli,
            enabled: true,
            command: "claude".to_string(),
            args: vec![],
            env: BTreeMap::new(),
            default_model: None,
            models: vec![
                "sonnet".to_string(),
                "opus".to_string(),
                "haiku".to_string(),
            ],
            description:
                "Anthropic Claude Code CLI (streaming JSON / permission prompts / session resume)"
                    .to_string(),
        },
        ProviderConfig {
            id: "codex".to_string(),
            name: "Codex".to_string(),
            kind: ProviderKind::CodexCli,
            enabled: true,
            command: "codex".to_string(),
            args: vec![],
            env: BTreeMap::new(),
            default_model: None,
            models: vec![
                "gpt-5.3-codex".to_string(),
                "gpt-5.4".to_string(),
                "o3".to_string(),
            ],
            description: "OpenAI Codex CLI (codex app-server)".to_string(),
        },
        ProviderConfig {
            id: "antigravity".to_string(),
            name: "Antigravity (ACP)".to_string(),
            kind: ProviderKind::Acp,
            enabled: true,
            command: "agy".to_string(),
            args: vec!["acp".to_string()],
            env: BTreeMap::new(),
            default_model: None,
            models: vec![],
            description: "Google Antigravity via Agent Client Protocol (agy acp)".to_string(),
        },
    ]
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            theme: ThemeMode::Dark,
            color_theme: ColorTheme::default(),
            ui_font: None,
            mono_font: None,
            font_size: None,
            language: Language::Ja,
            default_provider_id: default_provider_id(),
            auto_approve_tools: false,
            migrated_auto_approve: false,
            default_approval_mode: ApprovalMode::default(),
            notifications: true,
            submit_key: SubmitKey::default(),
            providers: default_providers(),
            project_exec: BTreeMap::new(),
            max_concurrent_sessions: None,
            default_handoff_mode: HandoffMode::default(),
            default_workdir_mode: WorkdirMode::default(),
        }
    }
}

impl AppConfig {
    pub fn load_or_default(paths: &AppPaths) -> Result<Self> {
        if !paths.config_file.exists() {
            let cfg = Self::default();
            cfg.save(paths)?;
            return Ok(cfg);
        }
        let raw = fs::read_to_string(&paths.config_file)
            .with_context(|| format!("Failed to read {}", paths.config_file.display()))?;
        let mut cfg: Self = toml::from_str(&raw)
            .with_context(|| format!("Failed to parse {}", paths.config_file.display()))?;
        cfg.migrate_auto_approve(&raw);
        Ok(cfg)
    }

    /// The old global auto-approve toggle becomes "accept edits", which keeps
    /// Codex's sandbox, rather than full access.
    fn migrate_auto_approve(&mut self, raw: &str) {
        let has_mode = toml::from_str::<toml::Value>(raw)
            .ok()
            .and_then(|v| v.get("default_approval_mode").cloned())
            .is_some();
        if self.auto_approve_tools && !has_mode {
            self.default_approval_mode = ApprovalMode::AcceptEdits;
            self.migrated_auto_approve = true;
        }
        self.auto_approve_tools = false;
    }

    pub fn save(&self, paths: &AppPaths) -> Result<()> {
        let serialized = toml::to_string_pretty(self)?;
        fs::write(&paths.config_file, serialized)
            .with_context(|| format!("Failed to write {}", paths.config_file.display()))?;
        Ok(())
    }

    pub fn exec_mode_for(&self, workdir: &str) -> ExecMode {
        self.project_exec.get(workdir).copied().unwrap_or_default()
    }

    pub fn set_exec_mode_for(&mut self, workdir: &str, mode: ExecMode) {
        if mode == ExecMode::Auto {
            self.project_exec.remove(workdir);
        } else {
            self.project_exec.insert(workdir.to_string(), mode);
        }
    }

    pub fn find_provider(&self, id: &str) -> Option<&ProviderConfig> {
        self.providers.iter().find(|p| p.id == id)
    }

    /// The configured font size, clamped to the supported range. A
    /// hand-edited out-of-range or non-finite value counts as unset.
    pub fn font_size(&self) -> Option<f32> {
        self.font_size
            .filter(|s| s.is_finite())
            .map(|s| s.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE))
    }

    pub fn max_concurrent_sessions(&self) -> Option<usize> {
        self.max_concurrent_sessions
            .filter(|&n| n > 0)
            .map(|n| n.clamp(MIN_CONCURRENT_SESSIONS, MAX_CONCURRENT_SESSIONS))
    }
}

/// Bounds of the UI font size setting, in px.
pub const MIN_FONT_SIZE: f32 = 10.0;
pub const MAX_FONT_SIZE: f32 = 24.0;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_auto_approve_migrates_to_accept_edits() {
        let raw = "auto_approve_tools = true\n";
        let mut cfg: AppConfig = toml::from_str(raw).unwrap();
        cfg.migrate_auto_approve(raw);
        assert_eq!(cfg.default_approval_mode, ApprovalMode::AcceptEdits);
        assert!(cfg.migrated_auto_approve);
        let saved = toml::to_string_pretty(&cfg).unwrap();
        assert!(!saved.contains("auto_approve_tools"));

        let raw = "auto_approve_tools = true\ndefault_approval_mode = \"plan\"\n";
        let mut cfg: AppConfig = toml::from_str(raw).unwrap();
        cfg.migrate_auto_approve(raw);
        assert_eq!(cfg.default_approval_mode, ApprovalMode::Plan);
        assert!(!cfg.migrated_auto_approve);

        let cfg: AppConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.default_approval_mode, ApprovalMode::Ask);
        assert!(cfg.notifications);
    }

    #[test]
    fn legacy_config_without_color_theme_loads() {
        let raw = "theme = \"light\"\nlanguage = \"en\"\n";
        let cfg: AppConfig = toml::from_str(raw).unwrap();
        assert_eq!(cfg.theme, ThemeMode::Light);
        assert_eq!(cfg.color_theme, ColorTheme::Ocean);
        assert_eq!(cfg.submit_key, SubmitKey::Enter);
        assert!(!cfg.providers.is_empty());
    }

    #[test]
    fn old_accent_key_is_ignored() {
        let raw = "theme = \"dark\"\naccent = \"purple\"\n";
        let cfg: AppConfig = toml::from_str(raw).unwrap();
        assert_eq!(cfg.theme, ThemeMode::Dark);
        assert_eq!(cfg.color_theme, ColorTheme::Ocean);
        let saved = toml::to_string_pretty(&cfg).unwrap();
        assert!(!saved.contains("accent"));
    }

    #[test]
    fn default_config_round_trips() {
        let mut cfg = AppConfig::default();
        cfg.color_theme = ColorTheme::Sakura;
        let raw = toml::to_string_pretty(&cfg).unwrap();
        let back: AppConfig = toml::from_str(&raw).unwrap();
        assert_eq!(back.color_theme, ColorTheme::Sakura);
        assert_eq!(back.providers.len(), cfg.providers.len());
    }

    #[test]
    fn unset_fonts_are_omitted_and_set_fonts_round_trip() {
        let cfg = AppConfig::default();
        let raw = toml::to_string_pretty(&cfg).unwrap();
        assert!(!raw.contains("ui_font"));
        assert!(!raw.contains("font_size"));

        let mut cfg = AppConfig::default();
        cfg.ui_font = Some("Noto Sans JP".into());
        cfg.mono_font = Some("JetBrains Mono".into());
        cfg.font_size = Some(15.0);
        let raw = toml::to_string_pretty(&cfg).unwrap();
        let back: AppConfig = toml::from_str(&raw).unwrap();
        assert_eq!(back.ui_font.as_deref(), Some("Noto Sans JP"));
        assert_eq!(back.mono_font.as_deref(), Some("JetBrains Mono"));
        assert_eq!(back.font_size(), Some(15.0));
    }

    #[test]
    fn font_size_is_clamped() {
        let mut cfg = AppConfig::default();
        cfg.font_size = Some(100.0);
        assert_eq!(cfg.font_size(), Some(MAX_FONT_SIZE));
        cfg.font_size = Some(1.0);
        assert_eq!(cfg.font_size(), Some(MIN_FONT_SIZE));
        cfg.font_size = Some(f32::NAN);
        assert_eq!(cfg.font_size(), None);
    }
}
