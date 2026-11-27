use anyhow::Result;
use same_acp::exec::ExecTarget;
use same_agent::{discover_external_skills, SkillSynchronizer};
use same_core::config::default_providers;
use same_core::skills::{resolve_skills, SkillManager, SkillScope};
use same_core::{
    EventBus, ProviderConfig, ProviderKind, SkillDeploymentMethod, SkillDeploymentStatus, Storage,
};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

struct TempDir(PathBuf);

impl TempDir {
    fn new(prefix: &str) -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(1);
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "same-agent-test-{prefix}-{}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn syncs_all_p0_providers_and_isolates_concurrent_projects() -> Result<()> {
    let tmp = TempDir::new("sync-p0");
    let storage = Arc::new(Storage::open_in_memory()?);
    let ws = storage.ensure_default_workspace()?;
    let proj_a = storage.create_project(&ws.id, "ProjA", "/tmp/proj-a", "")?;
    let proj_b = storage.create_project(&ws.id, "ProjB", "/tmp/proj-b", "")?;
    let events = EventBus::default();

    let mgr = SkillManager::new(tmp.0.clone(), storage.clone(), events.clone());
    let sync = SkillSynchronizer::new(tmp.0.clone(), storage.clone(), events);

    mgr.create_skill(
        "global-helper",
        "Global skill",
        SkillScope::Global,
        None,
        "# Global\n",
    )?;
    mgr.create_skill(
        "secret-a",
        "Only for project A",
        SkillScope::Project,
        Some(&proj_a.id),
        "# Secret A\n",
    )?;
    mgr.create_skill(
        "secret-b",
        "Only for project B",
        SkillScope::Project,
        Some(&proj_b.id),
        "# Secret B\n",
    )?;

    let all = mgr.list_skills()?;
    let res_a = resolve_skills(Some(&proj_a.id), &all, &[], &[]);
    let res_b = resolve_skills(Some(&proj_b.id), &all, &[], &[]);

    let providers = default_providers();
    let target = ExecTarget::Host;

    // 1. Claude Code: --plugin-dir with .claude-plugin/plugin.json + isolated project skills
    let claude = providers.iter().find(|p| p.id == "claude").unwrap();
    let plan_a = sync.sync_for_provider(claude, Some(&proj_a.id), &target, &res_a)?;
    let plan_b = sync.sync_for_provider(claude, Some(&proj_b.id), &target, &res_b)?;
    assert_eq!(plan_a.method, SkillDeploymentMethod::ClaudePluginDir);
    assert_eq!(plan_a.status, SkillDeploymentStatus::Synced);
    assert_eq!(plan_a.extra_args[0], "--plugin-dir");
    let stage_a = plan_a.staging_dir.as_ref().unwrap();
    let stage_b = plan_b.staging_dir.as_ref().unwrap();
    assert_ne!(stage_a, stage_b);
    assert!(stage_a.join(".claude-plugin/plugin.json").is_file());
    assert!(stage_a.join("skills/global-helper/SKILL.md").is_file());
    assert!(stage_a.join("skills/secret-a/SKILL.md").is_file());
    assert!(!stage_a.join("skills/secret-b").exists());
    assert!(stage_b.join("skills/secret-b/SKILL.md").is_file());
    assert!(!stage_b.join("skills/secret-a").exists());

    // 2. Codex: codex_extra_roots points to isolated skills dir
    let codex = providers.iter().find(|p| p.id == "codex").unwrap();
    let codex_plan = sync.sync_for_provider(codex, Some(&proj_a.id), &target, &res_a)?;
    assert_eq!(codex_plan.method, SkillDeploymentMethod::CodexExtraRoots);
    assert_eq!(codex_plan.codex_extra_roots.len(), 1);
    assert!(PathBuf::from(&codex_plan.codex_extra_roots[0])
        .join("secret-a/SKILL.md")
        .is_file());

    // 3. OpenCode: OPENCODE_CONFIG_CONTENT env var contains skills.paths
    let opencode = providers.iter().find(|p| p.id == "opencode").unwrap();
    let oc_plan = sync.sync_for_provider(opencode, Some(&proj_a.id), &target, &res_a)?;
    assert_eq!(oc_plan.method, SkillDeploymentMethod::OpenCodeConfigEnv);
    let env_json = oc_plan.extra_env.get("OPENCODE_CONFIG_CONTENT").unwrap();
    assert!(env_json.contains("skills"));
    assert!(env_json.contains("paths"));

    // 4. Antigravity: --add-dir with .agents/skills/<name>/SKILL.md
    let agy = providers.iter().find(|p| p.id == "antigravity").unwrap();
    let agy_plan = sync.sync_for_provider(agy, Some(&proj_a.id), &target, &res_a)?;
    assert_eq!(agy_plan.method, SkillDeploymentMethod::AntigravityAddDir);
    assert_eq!(agy_plan.extra_args[0], "--add-dir");
    assert!(agy_plan
        .staging_dir
        .as_ref()
        .unwrap()
        .join(".agents/skills/secret-a/SKILL.md")
        .is_file());

    // 5. Custom ACP: explicitly reports Unsupported with constraint note
    let custom = ProviderConfig {
        id: "my-acp".to_string(),
        name: "My Custom ACP".to_string(),
        kind: ProviderKind::Acp,
        enabled: true,
        command: "my-custom-acp".to_string(),
        args: vec![],
        env: BTreeMap::new(),
        default_model: None,
        models: vec![],
        description: String::new(),
    };
    let custom_plan = sync.sync_for_provider(&custom, Some(&proj_a.id), &target, &res_a)?;
    assert_eq!(custom_plan.method, SkillDeploymentMethod::Unsupported);
    assert_eq!(custom_plan.status, SkillDeploymentStatus::Unsupported);
    assert!(custom_plan.note.is_some());

    Ok(())
}

#[test]
fn detects_external_edit_conflict_and_preserves_unmanaged_files() -> Result<()> {
    let tmp = TempDir::new("conflict");
    let storage = Arc::new(Storage::open_in_memory()?);
    let events = EventBus::default();
    let mgr = SkillManager::new(tmp.0.clone(), storage.clone(), events.clone());
    let sync = SkillSynchronizer::new(tmp.0.clone(), storage.clone(), events);

    mgr.create_skill(
        "demo",
        "Demo skill",
        SkillScope::Global,
        None,
        "# Original\n",
    )?;
    let all = mgr.list_skills()?;
    let res = resolve_skills(None, &all, &[], &[]);
    let claude = default_providers()
        .into_iter()
        .find(|p| p.id == "claude")
        .unwrap();

    // First sync succeeds
    let plan1 = sync.sync_for_provider(&claude, None, &ExecTarget::Host, &res)?;
    assert_eq!(plan1.status, SkillDeploymentStatus::Synced);
    let stage = plan1.staging_dir.as_ref().unwrap();

    // Place an unmanaged directory next to SAME's skill
    let unmanaged = stage.join("skills/user-manual-dir");
    fs::create_dir_all(&unmanaged)?;
    fs::write(unmanaged.join("keep.txt"), "do not delete")?;

    // Externally edit the staged skill
    let staged_md = stage.join("skills/demo/SKILL.md");
    fs::write(
        &staged_md,
        "---\nname: demo\ndescription: Hand-edited\n---\n\n# Hand-edited\n",
    )?;

    // Second sync must detect Conflict and NOT overwrite the external edit or delete unmanaged dir
    let plan2 = sync.sync_for_provider(&claude, None, &ExecTarget::Host, &res)?;
    assert_eq!(plan2.status, SkillDeploymentStatus::Conflict);
    assert!(fs::read_to_string(&staged_md)?.contains("Hand-edited"));
    assert!(unmanaged.join("keep.txt").is_file());

    Ok(())
}

#[test]
fn migrates_legacy_manifest_hashes_without_false_conflict() -> Result<()> {
    let tmp = TempDir::new("legacy-hash");
    let storage = Arc::new(Storage::open_in_memory()?);
    let events = EventBus::default();
    let mgr = SkillManager::new(tmp.0.clone(), storage.clone(), events.clone());
    let sync = SkillSynchronizer::new(tmp.0.clone(), storage.clone(), events);

    let skill_a = mgr.create_skill("skill-a", "Skill A", SkillScope::Global, None, "# V1\n")?;
    let skill_b = mgr.create_skill(
        "skill-b",
        "Skill B",
        SkillScope::Global,
        None,
        "# Keep or Remove\n",
    )?;
    let all = mgr.list_skills()?;
    let res = resolve_skills(None, &all, &[], &[]);
    let claude = default_providers()
        .into_iter()
        .find(|p| p.id == "claude")
        .unwrap();

    let plan1 = sync.sync_for_provider(&claude, None, &ExecTarget::Host, &res)?;
    assert_eq!(plan1.status, SkillDeploymentStatus::Synced);
    let stage = plan1.staging_dir.as_ref().unwrap();
    let manifest_path = stage.join(".same-managed.json");

    // Rewrite manifest with legacy (pre-executable-bit) hashes to simulate an older SAME version.
    let legacy_a = same_core::skills::compute_legacy_dir_hash(&stage.join("skills/skill-a"))?;
    let legacy_b = same_core::skills::compute_legacy_dir_hash(&stage.join("skills/skill-b"))?;
    let modern_a = same_core::skills::compute_dir_hash(&stage.join("skills/skill-a"))?;
    assert_ne!(legacy_a, modern_a);
    fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&serde_json::json!({
            "skills": {
                "skill-a": legacy_a,
                "skill-b": legacy_b,
            }
        }))?,
    )?;

    // Edit skill-a and disable skill-b; re-sync must succeed without Conflict.
    mgr.update_skill_md(
        &skill_a.id,
        "---\nname: skill-a\ndescription: Skill A\n---\n\n# V2\n",
    )?;
    mgr.set_skill_enabled(&skill_b.id, false)?;

    let all2 = mgr.list_skills()?;
    let res2 = resolve_skills(None, &all2, &[], &[]);
    let plan2 = sync.sync_for_provider(&claude, None, &ExecTarget::Host, &res2)?;
    assert_eq!(plan2.status, SkillDeploymentStatus::Synced);
    assert!(fs::read_to_string(stage.join("skills/skill-a/SKILL.md"))?.contains("# V2"));
    assert!(!stage.join("skills/skill-b").exists());

    // Manifest should now store the modern hash for skill-a.
    let updated_manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&manifest_path)?)?;
    let expected_modern_a = same_core::skills::compute_dir_hash(&stage.join("skills/skill-a"))?;
    assert_eq!(
        updated_manifest["skills"]["skill-a"].as_str(),
        Some(expected_modern_a.as_str())
    );

    Ok(())
}

#[test]
fn wsl_target_translates_windows_staging_paths() {
    let target = ExecTarget::Wsl {
        distro: "Ubuntu".to_string(),
    };
    let win_path = PathBuf::from(r"C:\Users\alice\AppData\Roaming\same\deployments\claude");
    assert_eq!(
        target.to_target_path(&win_path),
        "/mnt/c/Users/alice/AppData/Roaming/same/deployments/claude"
    );
}

#[tokio::test]
async fn discovers_project_external_skills_read_only() -> Result<()> {
    let tmp = TempDir::new("discover");
    let proj_skill = tmp.0.join(".agents/skills/repo-guide");
    fs::create_dir_all(&proj_skill)?;
    fs::write(
        proj_skill.join("SKILL.md"),
        "---\nname: repo-guide\ndescription: Repo conventions\n---\n\n# Guide\n",
    )?;

    let found = discover_external_skills(&ExecTarget::Host, Some(&tmp.0)).await;
    assert!(found
        .iter()
        .any(|s| s.name == "repo-guide" && s.source_label == ".agents/skills"));
    Ok(())
}
