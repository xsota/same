use crate::models::{ProviderConfig, ProviderKind};
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
    #[serde(default)]
    pub language: Language,
    #[serde(default = "default_provider_id")]
    pub default_provider_id: String,
    #[serde(default)]
    pub auto_approve_tools: bool,
    #[serde(default = "default_providers")]
    pub providers: Vec<ProviderConfig>,
    /// Per-project overrides of [`ExecMode`], keyed by project folder.
    /// Projects not listed use [`ExecMode::Auto`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub project_exec: BTreeMap<String, ExecMode>,
}

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
            description: "Anthropic Claude Code CLI (streaming JSON / session resume)".to_string(),
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
            description: "OpenAI Codex CLI (codex exec --json)".to_string(),
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
            language: Language::Ja,
            default_provider_id: default_provider_id(),
            auto_approve_tools: false,
            providers: default_providers(),
            project_exec: BTreeMap::new(),
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
        let cfg: Self = toml::from_str(&raw)
            .with_context(|| format!("Failed to parse {}", paths.config_file.display()))?;
        Ok(cfg)
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
}
