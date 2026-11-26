use crate::events::{EventBus, SameEvent};
use crate::skills::validator::{
    compute_dir_hash, parse_skill_md, render_skill_md, slugify_skill_name,
    validate_relative_skill_path, validate_skill_dir, validate_skill_md, validate_skill_name,
    RelatedFileInfo, SkillValidationReport,
};
use crate::skills::{Skill, SkillAssignment, SkillScope, SkillSourceType};
use crate::storage::Storage;
use anyhow::{anyhow, bail, Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillManagerError {
    ValidationFailed(SkillValidationReport),
    AlreadyExists(String),
    NotFound(String),
    Other(String),
}

impl std::fmt::Display for SkillManagerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ValidationFailed(r) => write!(f, "Validation failed: {}", r.error_summary()),
            Self::AlreadyExists(n) => write!(f, "Skill '{n}' already exists in this scope"),
            Self::NotFound(id) => write!(f, "Skill '{id}' not found"),
            Self::Other(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for SkillManagerError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportConflictPolicy {
    /// Fail / skip if a skill with the same name already exists in the target scope.
    Skip,
    /// Rename the imported skill (e.g. `skill-name-2`) to avoid overwriting.
    Rename,
    /// Explicitly overwrite the existing SAME-managed skill.
    Overwrite,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportCandidate {
    pub name: String,
    pub description: String,
    pub source_path: PathBuf,
    pub is_single_file: bool,
    pub related_files: Vec<RelatedFileInfo>,
    pub has_executable_scripts: bool,
    pub collides_with_existing: bool,
    pub validation: SkillValidationReport,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportPreview {
    pub source_root: PathBuf,
    pub candidates: Vec<ImportCandidate>,
}

/// Manages SAME's canonical skill files on disk and metadata in SQLite.
#[derive(Clone)]
pub struct SkillManager {
    data_dir: PathBuf,
    storage: Arc<Storage>,
    events: EventBus,
}

impl SkillManager {
    pub fn new(data_dir: PathBuf, storage: Arc<Storage>, events: EventBus) -> Self {
        Self {
            data_dir,
            storage,
            events,
        }
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn storage(&self) -> &Arc<Storage> {
        &self.storage
    }

    pub fn global_skills_dir(&self) -> PathBuf {
        self.data_dir.join("skills").join("global")
    }

    pub fn project_skills_dir(&self, project_id: &str) -> PathBuf {
        self.data_dir
            .join("projects")
            .join(project_id)
            .join("skills")
    }

    pub fn skill_dir_for(&self, scope: SkillScope, project_id: Option<&str>, name: &str) -> Result<PathBuf> {
        validate_skill_name(name).map_err(|e| anyhow!(e))?;
        match scope {
            SkillScope::Global => Ok(self.global_skills_dir().join(name)),
            SkillScope::Project => {
                let pid = project_id
                    .filter(|p| !p.trim().is_empty())
                    .ok_or_else(|| anyhow!("Project skill requires a project_id"))?;
                Ok(self.project_skills_dir(pid).join(name))
            }
        }
    }

    pub fn list_skills(&self) -> Result<Vec<Skill>> {
        self.storage.list_skills()
    }

    pub fn get_skill(&self, skill_id: &str) -> Result<Option<Skill>> {
        self.storage.get_skill(skill_id)
    }

    pub fn search_skills(&self, query: &str) -> Result<Vec<Skill>> {
        let q = query.trim().to_ascii_lowercase();
        let all = self.storage.list_skills()?;
        if q.is_empty() {
            return Ok(all);
        }
        Ok(all
            .into_iter()
            .filter(|s| {
                s.name.to_ascii_lowercase().contains(&q)
                    || s.description.to_ascii_lowercase().contains(&q)
            })
            .collect())
    }

    /// Creates a new standard Agent Skills directory and `SKILL.md` (Section 9).
    pub fn create_skill(
        &self,
        name: &str,
        description: &str,
        scope: SkillScope,
        project_id: Option<&str>,
        instructions: &str,
    ) -> Result<Skill> {
        let name = name.trim();
        let description = description.trim();
        let body = if instructions.trim().is_empty() {
            format!("# {name}\n\nProvide step-by-step instructions here.\n")
        } else {
            instructions.to_string()
        };
        let md = render_skill_md(name, description, &BTreeMap::new(), &body);
        let (_, report) = validate_skill_md(&md, Some(name));
        if !report.is_valid() {
            bail!("{}", report.error_summary());
        }

        if self.find_by_scope_and_name(scope, project_id, name)?.is_some() {
            bail!("Skill '{name}' already exists in this scope");
        }

        let dir = self.skill_dir_for(scope, project_id, name)?;
        if dir.exists() {
            bail!("Skill directory already exists: {}", dir.display());
        }
        fs::create_dir_all(&dir)
            .with_context(|| format!("Failed to create skill directory {}", dir.display()))?;
        fs::write(dir.join("SKILL.md"), &md)
            .with_context(|| format!("Failed to write SKILL.md in {}", dir.display()))?;

        let content_hash = compute_dir_hash(&dir)?;
        let now = Utc::now();
        let skill = Skill {
            id: Uuid::new_v4().to_string(),
            name: name.to_string(),
            description: description.to_string(),
            scope,
            project_id: match scope {
                SkillScope::Global => None,
                SkillScope::Project => project_id.map(str::to_string),
            },
            path: dir.to_string_lossy().to_string(),
            source_type: SkillSourceType::Manual,
            source_uri: None,
            source_ref: None,
            source_revision: None,
            enabled: true,
            content_hash,
            created_at: now,
            updated_at: now,
        };
        self.storage.insert_skill(&skill)?;
        self.events.publish(SameEvent::SkillCreated {
            skill_id: skill.id.clone(),
        });
        Ok(skill)
    }

    /// Reads the canonical `SKILL.md` from disk.
    pub fn read_skill_md(&self, skill_id: &str) -> Result<String> {
        let skill = self
            .storage
            .get_skill(skill_id)?
            .ok_or_else(|| anyhow!("Skill '{skill_id}' not found"))?;
        let md_path = Path::new(&skill.path).join("SKILL.md");
        fs::read_to_string(&md_path)
            .with_context(|| format!("Failed to read {}", md_path.display()))
    }

    /// Validates and saves updated `SKILL.md` content. If `name:` in frontmatter
    /// was changed, renames the skill directory atomically after checking conflicts.
    pub fn update_skill_md(&self, skill_id: &str, new_content: &str) -> Result<Skill> {
        let mut skill = self
            .storage
            .get_skill(skill_id)?
            .ok_or_else(|| anyhow!("Skill '{skill_id}' not found"))?;

        let (parsed, report) = validate_skill_md(new_content, None);
        if !report.is_valid() {
            bail!("{}", report.error_summary());
        }
        let parsed = parsed.ok_or_else(|| anyhow!("Failed to parse SKILL.md"))?;

        let mut dir = PathBuf::from(&skill.path);
        if parsed.name != skill.name {
            if self
                .find_by_scope_and_name(skill.scope, skill.project_id.as_deref(), &parsed.name)?
                .is_some()
            {
                bail!("Skill '{}' already exists in this scope", parsed.name);
            }
            let new_dir =
                self.skill_dir_for(skill.scope, skill.project_id.as_deref(), &parsed.name)?;
            if new_dir.exists() {
                bail!("Target directory already exists: {}", new_dir.display());
            }
            if let Some(parent) = new_dir.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::rename(&dir, &new_dir).with_context(|| {
                format!(
                    "Failed to rename {} to {}",
                    dir.display(),
                    new_dir.display()
                )
            })?;
            dir = new_dir;
            skill.name = parsed.name;
            skill.path = dir.to_string_lossy().to_string();
        }

        fs::write(dir.join("SKILL.md"), new_content)
            .with_context(|| format!("Failed to write SKILL.md in {}", dir.display()))?;
        skill.description = parsed.description;
        skill.content_hash = compute_dir_hash(&dir)?;
        skill.updated_at = Utc::now();

        self.storage.update_skill(&skill)?;
        self.events.publish(SameEvent::SkillUpdated {
            skill_id: skill.id.clone(),
        });
        Ok(skill)
    }

    /// Renames a skill (updates frontmatter `name:` and directory name).
    pub fn rename_skill(&self, skill_id: &str, new_name: &str) -> Result<Skill> {
        let new_name = new_name.trim();
        validate_skill_name(new_name).map_err(|e| anyhow!(e))?;
        let raw = self.read_skill_md(skill_id)?;
        let parsed = parse_skill_md(&raw).map_err(|e| anyhow!(e))?;
        let updated_md = render_skill_md(
            new_name,
            &parsed.description,
            &parsed.extra_metadata,
            &parsed.body,
        );
        self.update_skill_md(skill_id, &updated_md)
    }

    /// Duplicates a skill into a new name and/or scope (Global <-> Project).
    pub fn duplicate_skill(
        &self,
        skill_id: &str,
        new_name: &str,
        target_scope: SkillScope,
        target_project_id: Option<&str>,
    ) -> Result<Skill> {
        let new_name = new_name.trim();
        validate_skill_name(new_name).map_err(|e| anyhow!(e))?;
        let src = self
            .storage
            .get_skill(skill_id)?
            .ok_or_else(|| anyhow!("Skill '{skill_id}' not found"))?;
        if self
            .find_by_scope_and_name(target_scope, target_project_id, new_name)?
            .is_some()
        {
            bail!("Skill '{new_name}' already exists in target scope");
        }
        let dst_dir = self.skill_dir_for(target_scope, target_project_id, new_name)?;
        if dst_dir.exists() {
            bail!("Target directory already exists: {}", dst_dir.display());
        }
        copy_dir_safe(Path::new(&src.path), &dst_dir)?;

        // Update SKILL.md frontmatter `name:` to match `new_name`.
        let md_path = dst_dir.join("SKILL.md");
        let raw = fs::read_to_string(&md_path)?;
        let parsed = parse_skill_md(&raw).map_err(|e| anyhow!(e))?;
        let updated_md = render_skill_md(
            new_name,
            &parsed.description,
            &parsed.extra_metadata,
            &parsed.body,
        );
        fs::write(&md_path, updated_md)?;

        let now = Utc::now();
        let copy = Skill {
            id: Uuid::new_v4().to_string(),
            name: new_name.to_string(),
            description: parsed.description,
            scope: target_scope,
            project_id: match target_scope {
                SkillScope::Global => None,
                SkillScope::Project => target_project_id.map(str::to_string),
            },
            path: dst_dir.to_string_lossy().to_string(),
            source_type: SkillSourceType::Manual,
            source_uri: src.source_uri,
            source_ref: src.source_ref,
            source_revision: src.source_revision,
            enabled: src.enabled,
            content_hash: compute_dir_hash(&dst_dir)?,
            created_at: now,
            updated_at: now,
        };
        self.storage.insert_skill(&copy)?;
        self.events.publish(SameEvent::SkillCreated {
            skill_id: copy.id.clone(),
        });
        Ok(copy)
    }

    /// Moves a skill between Global and Project scopes.
    pub fn move_skill(
        &self,
        skill_id: &str,
        target_scope: SkillScope,
        target_project_id: Option<&str>,
    ) -> Result<Skill> {
        let mut skill = self
            .storage
            .get_skill(skill_id)?
            .ok_or_else(|| anyhow!("Skill '{skill_id}' not found"))?;
        let normalized_pid = match target_scope {
            SkillScope::Global => None,
            SkillScope::Project => {
                let p = target_project_id
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| anyhow!("Target project_id is required"))?;
                Some(p.to_string())
            }
        };
        if skill.scope == target_scope && skill.project_id == normalized_pid {
            return Ok(skill);
        }
        if self
            .find_by_scope_and_name(target_scope, normalized_pid.as_deref(), &skill.name)?
            .is_some()
        {
            bail!(
                "Skill '{}' already exists in target scope",
                skill.name
            );
        }
        let dst_dir = self.skill_dir_for(target_scope, normalized_pid.as_deref(), &skill.name)?;
        if dst_dir.exists() {
            bail!(
                "Target directory already exists: {} (refusing to overwrite untracked directory)",
                dst_dir.display()
            );
        }
        if let Some(parent) = dst_dir.parent() {
            fs::create_dir_all(parent)?;
        }
        let src_dir = PathBuf::from(&skill.path);
        if fs::rename(&src_dir, &dst_dir).is_err() {
            copy_dir_safe(&src_dir, &dst_dir)?;
            fs::remove_dir_all(&src_dir)?;
        }

        skill.scope = target_scope;
        skill.project_id = normalized_pid;
        skill.path = dst_dir.to_string_lossy().to_string();
        skill.updated_at = Utc::now();
        self.storage.update_skill(&skill)?;
        self.events.publish(SameEvent::SkillUpdated {
            skill_id: skill.id.clone(),
        });
        Ok(skill)
    }

    /// Deletes a SAME-managed skill from disk and SQLite.
    /// Safety guard: only deletes paths located inside SAME's `data_dir`.
    pub fn delete_skill(&self, skill_id: &str) -> Result<()> {
        let skill = self
            .storage
            .get_skill(skill_id)?
            .ok_or_else(|| anyhow!("Skill '{skill_id}' not found"))?;
        let dir = PathBuf::from(&skill.path);
        if dir.exists() {
            let canon_data = fs::canonicalize(&self.data_dir)
                .unwrap_or_else(|_| self.data_dir.clone());
            let canon_dir = fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
            if canon_dir.starts_with(&canon_data) {
                fs::remove_dir_all(&canon_dir).with_context(|| {
                    format!("Failed to delete skill directory {}", canon_dir.display())
                })?;
            }
        }
        self.storage.delete_skill(skill_id)?;
        self.events.publish(SameEvent::SkillDeleted {
            skill_id: skill_id.to_string(),
        });
        Ok(())
    }

    pub fn set_skill_enabled(&self, skill_id: &str, enabled: bool) -> Result<Skill> {
        let mut skill = self
            .storage
            .get_skill(skill_id)?
            .ok_or_else(|| anyhow!("Skill '{skill_id}' not found"))?;
        skill.enabled = enabled;
        skill.updated_at = Utc::now();
        self.storage.update_skill(&skill)?;
        if enabled {
            self.events.publish(SameEvent::SkillEnabled {
                skill_id: skill.id.clone(),
            });
        } else {
            self.events.publish(SameEvent::SkillDisabled {
                skill_id: skill.id.clone(),
            });
        }
        Ok(skill)
    }

    pub fn list_assignments(&self, project_id: &str) -> Result<Vec<SkillAssignment>> {
        self.storage.list_skill_assignments(project_id)
    }

    pub fn set_project_assignment(
        &self,
        skill_id: &str,
        project_id: &str,
        enabled: bool,
    ) -> Result<()> {
        self.storage
            .upsert_skill_assignment(&SkillAssignment {
                skill_id: skill_id.to_string(),
                project_id: project_id.to_string(),
                enabled,
            })?;
        self.events.publish(SameEvent::SkillAssignmentChanged {
            skill_id: skill_id.to_string(),
            project_id: project_id.to_string(),
            enabled: Some(enabled),
        });
        Ok(())
    }

    pub fn clear_project_assignment(&self, skill_id: &str, project_id: &str) -> Result<()> {
        self.storage.delete_skill_assignment(skill_id, project_id)?;
        self.events.publish(SameEvent::SkillAssignmentChanged {
            skill_id: skill_id.to_string(),
            project_id: project_id.to_string(),
            enabled: None,
        });
        Ok(())
    }

    /// Lists supporting files inside a SAME-managed skill directory.
    pub fn list_related_files(&self, skill_id: &str) -> Result<Vec<RelatedFileInfo>> {
        let skill = self
            .storage
            .get_skill(skill_id)?
            .ok_or_else(|| anyhow!("Skill '{skill_id}' not found"))?;
        let (_, related, _) = validate_skill_dir(Path::new(&skill.path));
        Ok(related)
    }

    /// Copies a host file into a relative path inside a SAME-managed skill directory
    /// (e.g. `references/guide.md` or `scripts/helper.sh`). Rejects path traversal.
    pub fn add_related_file(
        &self,
        skill_id: &str,
        relative_path: &str,
        source_file: &Path,
    ) -> Result<Skill> {
        let clean_rel = validate_relative_skill_path(relative_path).map_err(|e| anyhow!(e))?;
        if clean_rel == Path::new("SKILL.md") {
            bail!("Use update_skill_md to edit SKILL.md");
        }
        let mut skill = self
            .storage
            .get_skill(skill_id)?
            .ok_or_else(|| anyhow!("Skill '{skill_id}' not found"))?;
        let dst = Path::new(&skill.path).join(&clean_rel);
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(source_file, &dst).with_context(|| {
            format!(
                "Failed to copy {} to {}",
                source_file.display(),
                dst.display()
            )
        })?;
        skill.content_hash = compute_dir_hash(Path::new(&skill.path))?;
        skill.updated_at = Utc::now();
        self.storage.update_skill(&skill)?;
        self.events.publish(SameEvent::SkillUpdated {
            skill_id: skill.id.clone(),
        });
        Ok(skill)
    }

    /// Deletes a supporting file from a SAME-managed skill directory.
    pub fn delete_related_file(&self, skill_id: &str, relative_path: &str) -> Result<Skill> {
        let clean_rel = validate_relative_skill_path(relative_path).map_err(|e| anyhow!(e))?;
        if clean_rel == Path::new("SKILL.md") {
            bail!("Cannot delete SKILL.md");
        }
        let mut skill = self
            .storage
            .get_skill(skill_id)?
            .ok_or_else(|| anyhow!("Skill '{skill_id}' not found"))?;
        let target = Path::new(&skill.path).join(&clean_rel);
        if target.is_file() {
            fs::remove_file(&target)?;
        }
        skill.content_hash = compute_dir_hash(Path::new(&skill.path))?;
        skill.updated_at = Utc::now();
        self.storage.update_skill(&skill)?;
        self.events.publish(SameEvent::SkillUpdated {
            skill_id: skill.id.clone(),
        });
        Ok(skill)
    }

    /// Inspects a local file or directory for import (Section 10.1 & 10.3).
    /// Supports:
    /// - A single `SKILL.md` file
    /// - A single skill directory containing `SKILL.md`
    /// - A parent directory containing multiple skill subdirectories
    /// Never executes any scripts.
    pub fn inspect_import_source(
        &self,
        source: &Path,
        target_scope: SkillScope,
        target_project_id: Option<&str>,
    ) -> Result<ImportPreview> {
        let existing_names: Vec<String> = self
            .storage
            .list_skills()?
            .into_iter()
            .filter(|s| {
                s.scope == target_scope
                    && match target_scope {
                        SkillScope::Global => true,
                        SkillScope::Project => s.project_id.as_deref() == target_project_id,
                    }
            })
            .map(|s| s.name)
            .collect();

        let mut candidates = Vec::new();
        if source.is_file() {
            let content = fs::read_to_string(source)
                .with_context(|| format!("Failed to read {}", source.display()))?;
            let (parsed, report) = validate_skill_md(&content, None);
            let name = parsed
                .as_ref()
                .map(|p| p.name.clone())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| {
                    source
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .map(slugify_skill_name)
                        .unwrap_or_else(|| "imported-skill".to_string())
                });
            let description = parsed
                .as_ref()
                .map(|p| p.description.clone())
                .unwrap_or_default();
            let collides = existing_names.iter().any(|n| n == &name)
                || self
                    .skill_dir_for(target_scope, target_project_id, &name)
                    .is_ok_and(|p| p.exists());
            candidates.push(ImportCandidate {
                name,
                description,
                source_path: source.to_path_buf(),
                is_single_file: true,
                related_files: Vec::new(),
                has_executable_scripts: false,
                collides_with_existing: collides,
                validation: report,
            });
        } else if source.is_dir() {
            if source.join("SKILL.md").is_file() {
                candidates.push(self.inspect_skill_dir(
                    source,
                    target_scope,
                    target_project_id,
                    &existing_names,
                ));
            } else {
                // Multi-skill directory: scan one level deep for directories containing SKILL.md.
                let mut entries: Vec<PathBuf> = fs::read_dir(source)?
                    .filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| p.is_dir() && p.join("SKILL.md").is_file())
                    .collect();
                entries.sort();
                for dir in entries {
                    candidates.push(self.inspect_skill_dir(
                        &dir,
                        target_scope,
                        target_project_id,
                        &existing_names,
                    ));
                }
            }
        } else {
            bail!("Import source does not exist: {}", source.display());
        }

        Ok(ImportPreview {
            source_root: source.to_path_buf(),
            candidates,
        })
    }

    fn inspect_skill_dir(
        &self,
        dir: &Path,
        target_scope: SkillScope,
        target_project_id: Option<&str>,
        existing_names: &[String],
    ) -> ImportCandidate {
        let (parsed, related, report) = validate_skill_dir(dir);
        let name = parsed
            .as_ref()
            .map(|p| p.name.clone())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| {
                dir.file_name()
                    .and_then(|s| s.to_str())
                    .map(slugify_skill_name)
                    .unwrap_or_else(|| "imported-skill".to_string())
            });
        let description = parsed
            .as_ref()
            .map(|p| p.description.clone())
            .unwrap_or_default();
        let has_executable_scripts = related.iter().any(|f| f.is_executable);
        let collides = existing_names.iter().any(|n| n == &name)
            || self
                .skill_dir_for(target_scope, target_project_id, &name)
                .is_ok_and(|p| p.exists());
        ImportCandidate {
            name,
            description,
            source_path: dir.to_path_buf(),
            is_single_file: false,
            related_files: related,
            has_executable_scripts,
            collides_with_existing: collides,
            validation: report,
        }
    }

    /// Imports one or more validated candidates into SAME's canonical storage (Section 10.1).
    pub fn import_candidates(
        &self,
        candidates: &[ImportCandidate],
        target_scope: SkillScope,
        target_project_id: Option<&str>,
        policy: ImportConflictPolicy,
        source_type: SkillSourceType,
    ) -> Result<Vec<Skill>> {
        let mut imported = Vec::new();
        for cand in candidates {
            if !cand.validation.is_valid() {
                bail!(
                    "Cannot import invalid skill '{}': {}",
                    cand.name,
                    cand.validation.error_summary()
                );
            }
            let existing =
                self.find_by_scope_and_name(target_scope, target_project_id, &cand.name)?;
            let initial_dst = self.skill_dir_for(target_scope, target_project_id, &cand.name)?;
            let untracked_collision = existing.is_none() && initial_dst.exists();

            let (final_name, overwrite_existing) = match (&existing, untracked_collision, policy) {
                (Some(_), _, ImportConflictPolicy::Skip)
                | (None, true, ImportConflictPolicy::Skip) => continue,
                (Some(_), _, ImportConflictPolicy::Rename)
                | (None, true, ImportConflictPolicy::Rename) => (
                    self.allocate_unique_name(target_scope, target_project_id, &cand.name)?,
                    None,
                ),
                (None, true, ImportConflictPolicy::Overwrite) => {
                    bail!(
                        "Refusing to overwrite untracked directory at {}",
                        initial_dst.display()
                    );
                }
                (Some(old), _, ImportConflictPolicy::Overwrite) => {
                    (cand.name.clone(), Some(old.clone()))
                }
                (None, false, _) => (cand.name.clone(), None),
            };

            let dst_dir = self.skill_dir_for(target_scope, target_project_id, &final_name)?;
            let parent_dir = dst_dir
                .parent()
                .ok_or_else(|| anyhow!("Invalid destination directory {}", dst_dir.display()))?;
            fs::create_dir_all(parent_dir)?;

            // Stage into a sibling temporary directory first so any read/copy failure
            // leaves the existing skill and its DB record untouched.
            let stage_dir = parent_dir.join(format!(".same-import-{}", Uuid::new_v4()));
            let stage_result: Result<()> = (|| {
                fs::create_dir_all(&stage_dir)?;
                if cand.is_single_file {
                    let raw = fs::read_to_string(&cand.source_path)?;
                    let parsed = parse_skill_md(&raw).map_err(|e| anyhow!(e))?;
                    let md = render_skill_md(
                        &final_name,
                        &parsed.description,
                        &parsed.extra_metadata,
                        &parsed.body,
                    );
                    fs::write(stage_dir.join("SKILL.md"), md)?;
                } else {
                    copy_dir_safe(&cand.source_path, &stage_dir)?;
                    if final_name != cand.name {
                        let md_path = stage_dir.join("SKILL.md");
                        let raw = fs::read_to_string(&md_path)?;
                        let parsed = parse_skill_md(&raw).map_err(|e| anyhow!(e))?;
                        let md = render_skill_md(
                            &final_name,
                            &parsed.description,
                            &parsed.extra_metadata,
                            &parsed.body,
                        );
                        fs::write(&md_path, md)?;
                    }
                }
                Ok(())
            })();
            if let Err(err) = stage_result {
                let _ = fs::remove_dir_all(&stage_dir);
                return Err(err);
            }

            let content_hash = match compute_dir_hash(&stage_dir) {
                Ok(h) => h,
                Err(err) => {
                    let _ = fs::remove_dir_all(&stage_dir);
                    return Err(err);
                }
            };

            let now = Utc::now();
            if let Some(mut old) = overwrite_existing {
                // Swap staged replacement into place safely; restore backup if rename fails.
                let backup_dir = parent_dir.join(format!(".same-backup-{}", Uuid::new_v4()));
                let had_old = dst_dir.exists();
                if had_old {
                    if let Err(err) = fs::rename(&dst_dir, &backup_dir) {
                        let _ = fs::remove_dir_all(&stage_dir);
                        return Err(err.into());
                    }
                }
                if let Err(err) = fs::rename(&stage_dir, &dst_dir) {
                    if had_old {
                        let _ = fs::rename(&backup_dir, &dst_dir);
                    }
                    let _ = fs::remove_dir_all(&stage_dir);
                    return Err(err.into());
                }
                if had_old {
                    let _ = fs::remove_dir_all(&backup_dir);
                }

                old.name = final_name;
                old.description = cand.description.clone();
                old.path = dst_dir.to_string_lossy().to_string();
                old.source_type = source_type;
                old.source_uri = Some(cand.source_path.to_string_lossy().to_string());
                old.content_hash = content_hash;
                old.updated_at = now;
                self.storage.update_skill(&old)?;
                self.events.publish(SameEvent::SkillImported {
                    skill_id: old.id.clone(),
                });
                imported.push(old);
            } else {
                if dst_dir.exists() {
                    let _ = fs::remove_dir_all(&stage_dir);
                    bail!("Destination directory already exists: {}", dst_dir.display());
                }
                if let Err(err) = fs::rename(&stage_dir, &dst_dir) {
                    let _ = fs::remove_dir_all(&stage_dir);
                    return Err(err.into());
                }
                let skill = Skill {
                    id: Uuid::new_v4().to_string(),
                    name: final_name,
                    description: cand.description.clone(),
                    scope: target_scope,
                    project_id: match target_scope {
                        SkillScope::Global => None,
                        SkillScope::Project => target_project_id.map(str::to_string),
                    },
                    path: dst_dir.to_string_lossy().to_string(),
                    source_type,
                    source_uri: Some(cand.source_path.to_string_lossy().to_string()),
                    source_ref: None,
                    source_revision: None,
                    enabled: true,
                    content_hash,
                    created_at: now,
                    updated_at: now,
                };
                self.storage.insert_skill(&skill)?;
                self.events.publish(SameEvent::SkillImported {
                    skill_id: skill.id.clone(),
                });
                imported.push(skill);
            }
        }
        Ok(imported)
    }

    /// Exports a SAME-managed skill directory (with all related files) into `dest_parent/<skill-name>/`.
    pub fn export_skill(&self, skill_id: &str, dest_parent: &Path) -> Result<PathBuf> {
        let skill = self
            .storage
            .get_skill(skill_id)?
            .ok_or_else(|| anyhow!("Skill '{skill_id}' not found"))?;
        let dst = dest_parent.join(&skill.name);
        if dst.exists() {
            bail!(
                "Export destination already exists: {} (refusing to overwrite)",
                dst.display()
            );
        }
        copy_dir_safe(Path::new(&skill.path), &dst)?;
        Ok(dst)
    }

    /// Re-scans all SAME-managed skills on disk to pick up external edits to `SKILL.md`
    /// or supporting files and refreshes cached name/description/hash in SQLite.
    pub fn refresh_from_disk(&self) -> Result<Vec<Skill>> {
        let mut skills = self.storage.list_skills()?;
        for skill in &mut skills {
            let dir = Path::new(&skill.path);
            if !dir.is_dir() {
                continue;
            }
            if let Ok(hash) = compute_dir_hash(dir) {
                if hash != skill.content_hash {
                    if let Ok(raw) = fs::read_to_string(dir.join("SKILL.md")) {
                        if let Ok(parsed) = parse_skill_md(&raw) {
                            if !parsed.description.is_empty() {
                                skill.description = parsed.description;
                            }
                        }
                    }
                    skill.content_hash = hash;
                    skill.updated_at = Utc::now();
                    let _ = self.storage.update_skill(skill);
                }
            }
        }
        Ok(skills)
    }

    fn find_by_scope_and_name(
        &self,
        scope: SkillScope,
        project_id: Option<&str>,
        name: &str,
    ) -> Result<Option<Skill>> {
        let all = self.storage.list_skills()?;
        Ok(all.into_iter().find(|s| {
            s.scope == scope
                && s.name == name
                && match scope {
                    SkillScope::Global => true,
                    SkillScope::Project => s.project_id.as_deref() == project_id,
                }
        }))
    }

    fn allocate_unique_name(
        &self,
        scope: SkillScope,
        project_id: Option<&str>,
        base: &str,
    ) -> Result<String> {
        for idx in 2..1000 {
            let suffix = format!("-{idx}");
            let max_base = crate::skills::validator::MAX_SKILL_NAME_LEN.saturating_sub(suffix.len());
            let trimmed: String = base.chars().take(max_base).collect();
            let candidate = format!("{}{suffix}", trimmed.trim_end_matches('-'));
            let disk_free = self
                .skill_dir_for(scope, project_id, &candidate)
                .is_ok_and(|p| !p.exists());
            if disk_free
                && self
                    .find_by_scope_and_name(scope, project_id, &candidate)?
                    .is_none()
            {
                return Ok(candidate);
            }
        }
        bail!("Could not allocate unique skill name for '{base}'")
    }
}

/// Recursively copies `src` into `dst`, rejecting symlinks that point outside `src`.
pub fn copy_dir_safe(src: &Path, dst: &Path) -> Result<()> {
    let canon_src = fs::canonicalize(src)
        .with_context(|| format!("Failed to canonicalize {}", src.display()))?;
    copy_dir_inner(&canon_src, &canon_src, dst)
}

fn copy_dir_inner(root: &Path, current: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst).with_context(|| format!("Failed to create {}", dst.display()))?;
    for entry in fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        let meta = fs::symlink_metadata(&path)?;
        let target_dst = dst.join(entry.file_name());
        if meta.file_type().is_symlink() {
            let resolved = fs::canonicalize(&path).with_context(|| {
                format!("Broken symlink inside skill directory: {}", path.display())
            })?;
            if !resolved.starts_with(root) {
                bail!(
                    "Symlink '{}' escapes skill directory (rejected)",
                    path.display()
                );
            }
            if resolved.is_file() {
                fs::copy(&resolved, &target_dst)?;
            }
            continue;
        }
        if meta.is_dir() {
            copy_dir_inner(root, &path, &target_dst)?;
        } else if meta.is_file() {
            fs::copy(&path, &target_dst)?;
        }
    }
    Ok(())
}
