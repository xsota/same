use crate::skills::{ExternalSkill, Skill, SkillAssignment, SkillOrigin, SkillScope};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Effective activation state of a SAME-managed skill in a given project context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectiveSkillState {
    /// Active and will be provided to compatible agents.
    Active,
    /// Disabled globally (for Global skills) or in the project (for Project skills).
    Disabled,
    /// A Global skill explicitly disabled for this project via SkillAssignment.
    DisabledInProject,
    /// A Global skill shadowed by an active Project skill with the same name.
    ShadowedByProject,
    /// A Global skill shadowed by a *disabled* Project skill with the same name.
    /// Per spec Section 11, SAME does NOT implicitly fall back to Global.
    ShadowedByDisabledProject,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedSkillEntry {
    pub skill: Skill,
    pub state: EffectiveSkillState,
    /// For Global skills in a project context: `Some(enabled)` when overridden by
    /// a `SkillAssignment`, `None` when inheriting the Global default.
    pub project_override: Option<bool>,
    /// True if an External/Built-in skill with the same name also exists.
    pub has_external_conflict: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillConflictKind {
    /// Global and Project SAME skills share a name (Project wins).
    ProjectShadowsGlobal,
    /// Project skill is disabled and blocks the same-named Global skill (no fallback).
    DisabledProjectBlocksGlobal,
    /// A SAME skill shares a name with an External or Built-in skill.
    ExternalNameCollision,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillConflict {
    pub skill_name: String,
    pub kind: SkillConflictKind,
    pub detail: String,
}

/// Complete resolution result used by both the UI and Agent Runtime (Section 11).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillResolution {
    pub project_id: Option<String>,
    /// Final list of active SAME skills to deliver to agents (unique by name).
    pub active_skills: Vec<Skill>,
    /// All SAME skills relevant to this context (Global + this project's Project skills),
    /// with their effective states and override metadata for the UI.
    pub entries: Vec<ResolvedSkillEntry>,
    /// Detected name collisions and precedence warnings.
    pub conflicts: Vec<SkillConflict>,
}

/// Resolves which SAME skills are active for `project_id` (or `None` for a
/// project-less standalone session), applying inheritance, project overrides,
/// Global vs Project precedence, and external skill collision detection.
pub fn resolve_skills(
    project_id: Option<&str>,
    all_skills: &[Skill],
    assignments: &[SkillAssignment],
    external_skills: &[ExternalSkill],
) -> SkillResolution {
    let assignment_map: HashMap<&str, bool> = match project_id {
        Some(pid) => assignments
            .iter()
            .filter(|a| a.project_id == pid)
            .map(|a| (a.skill_id.as_str(), a.enabled))
            .collect(),
        None => HashMap::new(),
    };

    let external_by_name: HashMap<&str, Vec<&ExternalSkill>> = {
        let mut m: HashMap<&str, Vec<&ExternalSkill>> = HashMap::new();
        for ext in external_skills {
            m.entry(ext.name.as_str()).or_default().push(ext);
        }
        m
    };

    let mut globals: Vec<&Skill> = all_skills
        .iter()
        .filter(|s| s.scope == SkillScope::Global)
        .collect();
    globals.sort_by(|a, b| a.name.cmp(&b.name));

    let mut projects: Vec<&Skill> = match project_id {
        Some(pid) => all_skills
            .iter()
            .filter(|s| s.scope == SkillScope::Project && s.project_id.as_deref() == Some(pid))
            .collect(),
        None => Vec::new(),
    };
    projects.sort_by(|a, b| a.name.cmp(&b.name));

    let project_by_name: HashMap<&str, &Skill> =
        projects.iter().map(|s| (s.name.as_str(), *s)).collect();

    let mut active_skills = Vec::new();
    let mut entries = Vec::new();
    let mut conflicts = Vec::new();

    // 1. Evaluate Project skills first (highest precedence among SAME skills).
    for skill in &projects {
        let has_external_conflict = external_by_name.contains_key(skill.name.as_str());
        let state = if skill.enabled {
            active_skills.push((*skill).clone());
            EffectiveSkillState::Active
        } else {
            EffectiveSkillState::Disabled
        };

        if has_external_conflict {
            if let Some(exts) = external_by_name.get(skill.name.as_str()) {
                let sources = format_external_sources(exts);
                conflicts.push(SkillConflict {
                    skill_name: skill.name.clone(),
                    kind: SkillConflictKind::ExternalNameCollision,
                    detail: format!(
                        "Project skill '{}' collides with external skill ({sources})",
                        skill.name
                    ),
                });
            }
        }

        entries.push(ResolvedSkillEntry {
            skill: (*skill).clone(),
            state,
            project_override: None,
            has_external_conflict,
        });
    }

    // 2. Evaluate Global skills (inheriting global enabled unless overridden per-project).
    for skill in &globals {
        let project_override = assignment_map.get(skill.id.as_str()).copied();
        let effective_enabled = project_override.unwrap_or(skill.enabled);
        let has_external_conflict = external_by_name.contains_key(skill.name.as_str());

        let state = if let Some(proj_skill) = project_by_name.get(skill.name.as_str()) {
            if proj_skill.enabled {
                conflicts.push(SkillConflict {
                    skill_name: skill.name.clone(),
                    kind: SkillConflictKind::ProjectShadowsGlobal,
                    detail: format!(
                        "Project skill '{}' overrides Global skill of the same name",
                        skill.name
                    ),
                });
                EffectiveSkillState::ShadowedByProject
            } else {
                // Explicit rule (Section 11): disabling a Project skill does NOT
                // implicitly fall back to the same-named Global skill.
                conflicts.push(SkillConflict {
                    skill_name: skill.name.clone(),
                    kind: SkillConflictKind::DisabledProjectBlocksGlobal,
                    detail: format!(
                        "Project skill '{}' is disabled; Global skill is not applied (no implicit fallback)",
                        skill.name
                    ),
                });
                EffectiveSkillState::ShadowedByDisabledProject
            }
        } else if effective_enabled {
            active_skills.push((*skill).clone());
            if has_external_conflict {
                if let Some(exts) = external_by_name.get(skill.name.as_str()) {
                    let sources = format_external_sources(exts);
                    conflicts.push(SkillConflict {
                        skill_name: skill.name.clone(),
                        kind: SkillConflictKind::ExternalNameCollision,
                        detail: format!(
                            "Global skill '{}' collides with external skill ({sources})",
                            skill.name
                        ),
                    });
                }
            }
            EffectiveSkillState::Active
        } else if project_override == Some(false) {
            EffectiveSkillState::DisabledInProject
        } else {
            EffectiveSkillState::Disabled
        };

        entries.push(ResolvedSkillEntry {
            skill: (*skill).clone(),
            state,
            project_override,
            has_external_conflict,
        });
    }

    active_skills.sort_by(|a, b| a.name.cmp(&b.name));

    SkillResolution {
        project_id: project_id.map(str::to_string),
        active_skills,
        entries,
        conflicts,
    }
}

fn format_external_sources(exts: &[&ExternalSkill]) -> String {
    exts.iter()
        .map(|e| {
            let tag = match e.origin {
                SkillOrigin::BuiltIn => "built-in",
                _ => "external",
            };
            format!("{tag}: {} [{}]", e.source_label, e.provider_id)
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::SkillSourceType;
    use chrono::Utc;

    fn mk_skill(name: &str, scope: SkillScope, project_id: Option<&str>, enabled: bool) -> Skill {
        let now = Utc::now();
        Skill {
            id: format!("{}-{}", scope.as_str(), name),
            name: name.to_string(),
            description: format!("Desc for {name}"),
            scope,
            project_id: project_id.map(str::to_string),
            path: format!("/tmp/{name}"),
            source_type: SkillSourceType::Manual,
            source_uri: None,
            source_ref: None,
            source_revision: None,
            enabled,
            content_hash: "abc".to_string(),
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn project_overrides_global_and_disabled_project_blocks_fallback() {
        let skills = vec![
            mk_skill("shared", SkillScope::Global, None, true),
            mk_skill("shared", SkillScope::Project, Some("p1"), true),
            mk_skill("blocked", SkillScope::Global, None, true),
            mk_skill("blocked", SkillScope::Project, Some("p1"), false),
            mk_skill("global-only", SkillScope::Global, None, true),
            mk_skill("other-proj", SkillScope::Project, Some("p2"), true),
        ];

        let res = resolve_skills(Some("p1"), &skills, &[], &[]);
        let active_names: Vec<&str> = res.active_skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(active_names, vec!["global-only", "shared"]);

        // `shared` came from Project, not Global.
        let shared = res.active_skills.iter().find(|s| s.name == "shared").unwrap();
        assert_eq!(shared.scope, SkillScope::Project);

        // `blocked` must NOT fall back to Global!
        assert!(!active_names.contains(&"blocked"));
        assert!(res
            .conflicts
            .iter()
            .any(|c| c.skill_name == "blocked" && c.kind == SkillConflictKind::DisabledProjectBlocksGlobal));
        assert!(res
            .conflicts
            .iter()
            .any(|c| c.skill_name == "shared" && c.kind == SkillConflictKind::ProjectShadowsGlobal));
    }

    #[test]
    fn global_assignment_override_works_per_project() {
        let g = mk_skill("lint", SkillScope::Global, None, true);
        let g_disabled = mk_skill("opt-in", SkillScope::Global, None, false);
        let assignments = vec![
            SkillAssignment {
                skill_id: g.id.clone(),
                project_id: "p1".to_string(),
                enabled: false,
            },
            SkillAssignment {
                skill_id: g_disabled.id.clone(),
                project_id: "p1".to_string(),
                enabled: true,
            },
        ];

        let res_p1 = resolve_skills(Some("p1"), &[g.clone(), g_disabled.clone()], &assignments, &[]);
        let names_p1: Vec<&str> = res_p1.active_skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names_p1, vec!["opt-in"]);

        let res_p2 = resolve_skills(Some("p2"), &[g, g_disabled], &assignments, &[]);
        let names_p2: Vec<&str> = res_p2.active_skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names_p2, vec!["lint"]);
    }

    #[test]
    fn standalone_session_uses_only_global_skills() {
        let skills = vec![
            mk_skill("g1", SkillScope::Global, None, true),
            mk_skill("p1-only", SkillScope::Project, Some("p1"), true),
        ];
        let res = resolve_skills(None, &skills, &[], &[]);
        assert_eq!(res.active_skills.len(), 1);
        assert_eq!(res.active_skills[0].name, "g1");
    }

    #[test]
    fn detects_external_name_collision() {
        let skills = vec![mk_skill("deploy", SkillScope::Global, None, true)];
        let externals = vec![ExternalSkill {
            name: "deploy".to_string(),
            description: "External deploy".to_string(),
            origin: SkillOrigin::External,
            provider_id: "claude".to_string(),
            source_label: "~/.claude/skills".to_string(),
            path: "/home/u/.claude/skills/deploy".to_string(),
            execution_target: "Local".to_string(),
            has_scripts: false,
            related_files: vec![],
        }];
        let res = resolve_skills(Some("p1"), &skills, &[], &externals);
        assert!(res
            .conflicts
            .iter()
            .any(|c| c.kind == SkillConflictKind::ExternalNameCollision && c.skill_name == "deploy"));
        assert!(res.entries[0].has_external_conflict);
    }
}
