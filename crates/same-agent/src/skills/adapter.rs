use anyhow::{Context, Result};
use chrono::Utc;
use same_acp::exec::ExecTarget;
use same_core::skills::manager::copy_dir_safe;
use same_core::skills::validator::{compute_dir_hash, Sha256};
use same_core::{
    EventBus, ProviderConfig, ProviderKind, SameEvent, SkillDeployment, SkillDeploymentMethod,
    SkillDeploymentStatus, SkillResolution, Storage,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

fn staging_lock(staging_root: &Path) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();
    let map = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = map.lock().unwrap();
    guard
        .entry(staging_root.to_path_buf())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

const MANIFEST_FILE: &str = ".same-managed.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct StagingManifest {
    /// Map of `skill_name -> deployed_content_hash` for skills managed by SAME.
    skills: BTreeMap<String, String>,
}

/// Runtime skill delivery instructions produced by [`SkillSynchronizer`] for one
/// provider + project + execution target.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillDeliveryPlan {
    pub provider_id: String,
    pub method: SkillDeploymentMethod,
    /// Extra CLI arguments to append when spawning the provider (e.g. Claude `--plugin-dir`, Antigravity `--add-dir`).
    pub extra_args: Vec<String>,
    /// Extra environment variables to set when spawning the provider (e.g. OpenCode `OPENCODE_CONFIG_CONTENT`).
    pub extra_env: BTreeMap<String, String>,
    /// Extra skill root directories to register on Codex app-server via `skills/extraRoots/set`.
    pub codex_extra_roots: Vec<String>,
    /// Host path to the staging root, if any.
    pub staging_dir: Option<PathBuf>,
    /// Combined hash of all synced skills (used for process pooling / restart detection).
    pub deployment_hash: String,
    pub status: SkillDeploymentStatus,
    pub note: Option<String>,
}

fn command_basename(command: &str) -> String {
    let base = command.rsplit(['/', '\\']).next().unwrap_or(command);
    base.strip_suffix(".exe")
        .or_else(|| base.strip_suffix(".cmd"))
        .unwrap_or(base)
        .to_ascii_lowercase()
}

/// Selects the native, non-invasive skill delivery method for a provider.
pub fn provider_skill_method(provider: &ProviderConfig) -> SkillDeploymentMethod {
    match provider.kind {
        ProviderKind::ClaudeCli => SkillDeploymentMethod::ClaudePluginDir,
        ProviderKind::CodexCli => SkillDeploymentMethod::CodexExtraRoots,
        ProviderKind::Acp => {
            let cmd = command_basename(&provider.command);
            if provider.id == "opencode" || cmd == "opencode" {
                SkillDeploymentMethod::OpenCodeConfigEnv
            } else if provider.id == "antigravity" || cmd == "agy" {
                SkillDeploymentMethod::AntigravityAddDir
            } else {
                SkillDeploymentMethod::Unsupported
            }
        }
    }
}

/// Synchronizes resolved SAME skills into isolated per-provider / per-project /
/// per-target staging directories inside SAME's data directory.
///
/// Key guarantees (Sections 12, 15, 16, 19):
/// - Never writes to or modifies the user's `~/.claude`, `~/.codex`, `~/.config/opencode`,
///   `~/.gemini`, or project repository directories.
/// - Complete isolation between concurrently running projects.
/// - Idempotent: unchanged skills (matching content hash) are not rewritten.
/// - Conflict detection: if a staged skill directory was externally modified,
///   SAME flags `Conflict` instead of silently overwriting.
#[derive(Clone)]
pub struct SkillSynchronizer {
    data_dir: PathBuf,
    storage: Arc<Storage>,
    events: EventBus,
}

impl SkillSynchronizer {
    pub fn new(data_dir: PathBuf, storage: Arc<Storage>, events: EventBus) -> Self {
        Self {
            data_dir,
            storage,
            events,
        }
    }

    pub fn sync_for_provider(
        &self,
        provider: &ProviderConfig,
        project_id: Option<&str>,
        target: &ExecTarget,
        resolution: &SkillResolution,
    ) -> Result<SkillDeliveryPlan> {
        let method = provider_skill_method(provider);
        let project_key = project_id.unwrap_or("global");
        let project_db_key = project_id.unwrap_or("");
        let target_label = target.label();
        let target_key = sanitize_key(&target_label);

        self.events.publish(SameEvent::SkillSyncStarted {
            provider_id: provider.id.clone(),
            project_id: project_id.map(str::to_string),
        });

        if method == SkillDeploymentMethod::Unsupported {
            let note = format!(
                "{} (Custom ACP) does not advertise a known native skill directory override; configure skills directly in the agent.",
                provider.name
            );
            for entry in &resolution.entries {
                let _ = self.storage.upsert_skill_deployment(&SkillDeployment {
                    skill_id: entry.skill.id.clone(),
                    provider_id: provider.id.clone(),
                    project_id: project_db_key.to_string(),
                    execution_target: target_label.clone(),
                    deployment_path: String::new(),
                    deployment_method: method,
                    deployed_hash: String::new(),
                    status: SkillDeploymentStatus::Unsupported,
                    last_error: Some(note.clone()),
                    last_synced_at: Some(Utc::now()),
                });
            }
            self.events.publish(SameEvent::SkillSyncCompleted {
                provider_id: provider.id.clone(),
                project_id: project_id.map(str::to_string),
                synced_count: 0,
            });
            return Ok(SkillDeliveryPlan {
                provider_id: provider.id.clone(),
                method,
                status: SkillDeploymentStatus::Unsupported,
                note: Some(note),
                ..Default::default()
            });
        }

        let staging_root = self
            .data_dir
            .join("deployments")
            .join(sanitize_key(&provider.id))
            .join(sanitize_key(project_key))
            .join(&target_key);

        let skills_root = match method {
            SkillDeploymentMethod::ClaudePluginDir => staging_root.join("skills"),
            SkillDeploymentMethod::AntigravityAddDir => {
                staging_root.join(".agents").join("skills")
            }
            SkillDeploymentMethod::CodexExtraRoots
            | SkillDeploymentMethod::OpenCodeConfigEnv
            | SkillDeploymentMethod::StagingCopy => staging_root.join("skills"),
            SkillDeploymentMethod::Unsupported => unreachable!(),
        };

        let lock = staging_lock(&staging_root);
        let _sync_guard = lock.lock().unwrap();

        let sync_res = self.perform_staging_sync(
            provider,
            method,
            project_db_key,
            &target_label,
            target,
            &staging_root,
            &skills_root,
            resolution,
        );

        match sync_res {
            Ok((plan, synced_count)) => {
                self.events.publish(SameEvent::SkillSyncCompleted {
                    provider_id: provider.id.clone(),
                    project_id: project_id.map(str::to_string),
                    synced_count,
                });
                Ok(plan)
            }
            Err(err) => {
                let err_msg = format!("{err:#}");
                for entry in &resolution.entries {
                    let _ = self.storage.upsert_skill_deployment(&SkillDeployment {
                        skill_id: entry.skill.id.clone(),
                        provider_id: provider.id.clone(),
                        project_id: project_db_key.to_string(),
                        execution_target: target_label.clone(),
                        deployment_path: staging_root.to_string_lossy().to_string(),
                        deployment_method: method,
                        deployed_hash: String::new(),
                        status: SkillDeploymentStatus::Error,
                        last_error: Some(err_msg.clone()),
                        last_synced_at: Some(Utc::now()),
                    });
                }
                self.events.publish(SameEvent::SkillSyncFailed {
                    provider_id: provider.id.clone(),
                    project_id: project_id.map(str::to_string),
                    error: err_msg,
                });
                Err(err)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn perform_staging_sync(
        &self,
        provider: &ProviderConfig,
        method: SkillDeploymentMethod,
        project_db_key: &str,
        target_label: &str,
        target: &ExecTarget,
        staging_root: &Path,
        skills_root: &Path,
        resolution: &SkillResolution,
    ) -> Result<(SkillDeliveryPlan, usize)> {
        fs::create_dir_all(skills_root)
            .with_context(|| format!("Failed to create staging dir {}", skills_root.display()))?;

        let manifest_path = staging_root.join(MANIFEST_FILE);
        let mut manifest: StagingManifest = if manifest_path.is_file() {
            fs::read_to_string(&manifest_path)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default()
        } else {
            StagingManifest::default()
        };

        let active_names: HashSet<&str> = resolution
            .active_skills
            .iter()
            .map(|s| s.name.as_str())
            .collect();

        let mut synced_count = 0usize;
        let mut has_conflict = false;

        // Remove previously SAME-managed skills that are no longer active.
        // Never touch directories not listed in `.same-managed.json`, and never
        // silently delete a stale staged skill that was externally edited.
        let stale: Vec<(String, String)> = manifest
            .skills
            .iter()
            .filter(|(name, _)| !active_names.contains(name.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (name, prev_hash) in stale {
            let dir = skills_root.join(&name);
            if !dir.exists() {
                manifest.skills.remove(&name);
                continue;
            }
            if dir.is_dir() {
                let existing_hash = compute_dir_hash(&dir).unwrap_or_default();
                if existing_hash != prev_hash {
                    has_conflict = true;
                    let detail = format!(
                        "Staged skill '{}' for {} was modified externally; refusing to delete",
                        name, provider.name
                    );
                    self.events.publish(SameEvent::SkillConflictDetected {
                        skill_name: name,
                        detail,
                    });
                    continue;
                }
                fs::remove_dir_all(&dir).with_context(|| {
                    format!("Failed to remove stale staged skill {}", dir.display())
                })?;
                manifest.skills.remove(&name);
            }
        }

        let mut combined_hasher = Sha256::new();
        let now = Utc::now();

        for skill in &resolution.active_skills {
            let src_dir = Path::new(&skill.path);
            let dst_dir = skills_root.join(&skill.name);
            let current_src_hash = compute_dir_hash(src_dir).unwrap_or_else(|_| skill.content_hash.clone());
            let prev_deployed = manifest.skills.get(&skill.name).cloned();

            let mut status = SkillDeploymentStatus::Synced;
            let mut last_error = None;

            if dst_dir.is_dir() {
                let existing_hash = compute_dir_hash(&dst_dir).unwrap_or_default();
                if let Some(prev) = &prev_deployed {
                    if existing_hash != *prev && existing_hash != current_src_hash {
                        // External edit detected inside staging dir: flag conflict, do not overwrite.
                        status = SkillDeploymentStatus::Conflict;
                        has_conflict = true;
                        let detail = format!(
                            "Staged skill '{}' for {} was modified externally; skipping overwrite",
                            skill.name, provider.name
                        );
                        last_error = Some(detail.clone());
                        self.events.publish(SameEvent::SkillConflictDetected {
                            skill_name: skill.name.clone(),
                            detail,
                        });
                    } else if existing_hash != current_src_hash {
                        fs::remove_dir_all(&dst_dir)?;
                        copy_dir_safe(src_dir, &dst_dir)?;
                        manifest
                            .skills
                            .insert(skill.name.clone(), current_src_hash.clone());
                    }
                } else {
                    // Unmanaged directory at target path: treat as conflict and never
                    // claim ownership of it (even if bytes happen to match).
                    status = SkillDeploymentStatus::Conflict;
                    has_conflict = true;
                    let detail = format!(
                        "Unmanaged directory exists at {} for skill '{}'",
                        dst_dir.display(),
                        skill.name
                    );
                    last_error = Some(detail.clone());
                    self.events.publish(SameEvent::SkillConflictDetected {
                        skill_name: skill.name.clone(),
                        detail,
                    });
                }
            } else {
                copy_dir_safe(src_dir, &dst_dir)?;
                manifest
                    .skills
                    .insert(skill.name.clone(), current_src_hash.clone());
            }

            if status == SkillDeploymentStatus::Synced {
                synced_count += 1;
            }
            combined_hasher.update(skill.name.as_bytes());
            combined_hasher.update(b":");
            combined_hasher.update(current_src_hash.as_bytes());
            combined_hasher.update(b"\n");

            self.storage.upsert_skill_deployment(&SkillDeployment {
                skill_id: skill.id.clone(),
                provider_id: provider.id.clone(),
                project_id: project_db_key.to_string(),
                execution_target: target_label.to_string(),
                deployment_path: dst_dir.to_string_lossy().to_string(),
                deployment_method: method,
                deployed_hash: current_src_hash,
                status,
                last_error,
                last_synced_at: Some(now),
            })?;
        }

        // Record disabled/shadowed skills so the UI shows accurate status.
        for entry in &resolution.entries {
            if active_names.contains(entry.skill.name.as_str())
                && resolution
                    .active_skills
                    .iter()
                    .any(|a| a.id == entry.skill.id)
            {
                continue;
            }
            self.storage.upsert_skill_deployment(&SkillDeployment {
                skill_id: entry.skill.id.clone(),
                provider_id: provider.id.clone(),
                project_id: project_db_key.to_string(),
                execution_target: target_label.to_string(),
                deployment_path: String::new(),
                deployment_method: method,
                deployed_hash: String::new(),
                status: SkillDeploymentStatus::Disabled,
                last_error: None,
                last_synced_at: Some(now),
            })?;
        }

        // Ensure Claude Code plugin manifest exists when using ClaudePluginDir.
        if method == SkillDeploymentMethod::ClaudePluginDir {
            let plugin_meta_dir = staging_root.join(".claude-plugin");
            fs::create_dir_all(&plugin_meta_dir)?;
            let plugin_json_path = plugin_meta_dir.join("plugin.json");
            if !plugin_json_path.is_file() {
                let manifest_json = serde_json::to_string_pretty(&json!({
                    "name": "same-skills",
                    "version": "1.0.0",
                    "description": "Agent Skills managed by SAME"
                }))?;
                fs::write(&plugin_json_path, manifest_json)?;
            }
        }

        fs::write(&manifest_path, serde_json::to_string_pretty(&manifest)?)?;

        let deployment_hash = combined_hasher.finalize_hex();
        let target_staging_root = target.to_target_path(staging_root);
        let target_skills_root = target.to_target_path(skills_root);

        let mut extra_args = Vec::new();
        let mut extra_env = BTreeMap::new();
        let mut codex_extra_roots = Vec::new();

        if !has_conflict && !resolution.active_skills.is_empty() {
            match method {
                SkillDeploymentMethod::ClaudePluginDir => {
                    extra_args.push("--plugin-dir".to_string());
                    extra_args.push(target_staging_root);
                }
                SkillDeploymentMethod::CodexExtraRoots => {
                    codex_extra_roots.push(target_skills_root);
                }
                SkillDeploymentMethod::OpenCodeConfigEnv => {
                    let inherited = if *target == ExecTarget::Host {
                        std::env::var("OPENCODE_CONFIG_CONTENT").ok()
                    } else {
                        None
                    };
                    let existing = provider
                        .env
                        .get("OPENCODE_CONFIG_CONTENT")
                        .map(String::as_str)
                        .or(inherited.as_deref());
                    let merged = merge_opencode_config_env(existing, &target_skills_root);
                    extra_env.insert("OPENCODE_CONFIG_CONTENT".to_string(), merged);
                }
                SkillDeploymentMethod::AntigravityAddDir => {
                    extra_args.push("--add-dir".to_string());
                    extra_args.push(target_staging_root);
                }
                SkillDeploymentMethod::StagingCopy | SkillDeploymentMethod::Unsupported => {}
            }
        }

        let plan = SkillDeliveryPlan {
            provider_id: provider.id.clone(),
            method,
            extra_args,
            extra_env,
            codex_extra_roots,
            staging_dir: Some(staging_root.to_path_buf()),
            deployment_hash,
            status: if has_conflict {
                SkillDeploymentStatus::Conflict
            } else {
                SkillDeploymentStatus::Synced
            },
            note: None,
        };

        Ok((plan, synced_count))
    }
}

/// Merges `skills.paths` into any existing `OPENCODE_CONFIG_CONTENT` JSON without
/// clobbering the user's other inline config keys.
pub fn merge_opencode_config_env(existing: Option<&str>, skills_dir: &str) -> String {
    let mut root = existing
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .filter(|v| v.is_object())
        .unwrap_or_else(|| json!({}));

    let obj = root.as_object_mut().expect("object");
    let skills_entry = obj.entry("skills").or_insert_with(|| json!({}));
    if !skills_entry.is_object() {
        *skills_entry = json!({});
    }
    let skills_obj = skills_entry.as_object_mut().expect("skills object");
    let paths_entry = skills_obj.entry("paths").or_insert_with(|| json!([]));
    if !paths_entry.is_array() {
        *paths_entry = json!([]);
    }
    let arr = paths_entry.as_array_mut().expect("paths array");
    if !arr.iter().any(|v| v.as_str() == Some(skills_dir)) {
        arr.push(Value::String(skills_dir.to_string()));
    }

    serde_json::to_string(&root).unwrap_or_else(|_| {
        format!(
            r#"{{"skills":{{"paths":[{}]}}}}"#,
            serde_json::to_string(skills_dir).unwrap_or_default()
        )
    })
}

fn sanitize_key(raw: &str) -> String {
    let s: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() {
        "default".to_string()
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_opencode_config_env_preserving_existing_keys() {
        let merged = merge_opencode_config_env(
            Some(r#"{"model":"gpt-5","skills":{"paths":["/existing"]}}"#),
            "/same/skills",
        );
        let parsed: Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(parsed["model"], "gpt-5");
        let paths = parsed["skills"]["paths"].as_array().unwrap();
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0], "/existing");
        assert_eq!(paths[1], "/same/skills");
    }
}
