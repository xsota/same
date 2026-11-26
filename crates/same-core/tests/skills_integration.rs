use anyhow::Result;
use same_core::skills::{
    resolve_skills, EffectiveSkillState, ImportConflictPolicy, SkillConflictKind, SkillManager,
    SkillScope, SkillSourceType,
};
use same_core::{EventBus, SameEvent, Storage};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use uuid::Uuid;

struct TempDir(PathBuf);

impl TempDir {
    fn new(prefix: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("same-test-{prefix}-{}", Uuid::new_v4()));
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
fn skill_full_lifecycle_and_events() -> Result<()> {
    let tmp = TempDir::new("lifecycle");
    let storage = Arc::new(Storage::open_in_memory()?);
    let ws = storage.ensure_default_workspace()?;
    let proj = storage.create_project(&ws.id, "Alpha", "/tmp/alpha", "")?;
    let events = EventBus::default();
    let mut rx = events.subscribe();

    let mgr = SkillManager::new(tmp.0.clone(), storage.clone(), events);

    // 1. Create Global & Project skills
    let g = mgr.create_skill(
        "rust-review",
        "Reviews Rust PRs carefully",
        SkillScope::Global,
        None,
        "# Rust Review\n\nCheck ownership and error handling.",
    )?;
    assert_eq!(g.scope, SkillScope::Global);
    assert!(PathBuf::from(&g.path).join("SKILL.md").is_file());

    let p = mgr.create_skill(
        "release-checklist",
        "Alpha release checklist",
        SkillScope::Project,
        Some(&proj.id),
        "# Release\n\nRun cargo test.",
    )?;
    assert_eq!(p.project_id.as_deref(), Some(proj.id.as_str()));

    // Verify SkillCreated events fired
    assert!(matches!(rx.try_recv(), Ok(SameEvent::SkillCreated { .. })));
    assert!(matches!(rx.try_recv(), Ok(SameEvent::SkillCreated { .. })));

    // 2. Edit SKILL.md preserving unknown metadata
    let updated_md = "---\nname: rust-review\ndescription: Updated Rust review\nauthor: same-team\n---\n\n# Updated\n\nBody.\n";
    let g2 = mgr.update_skill_md(&g.id, updated_md)?;
    assert_eq!(g2.description, "Updated Rust review");
    assert_ne!(g2.content_hash, g.content_hash);

    // 3. Rename
    let g3 = mgr.rename_skill(&g.id, "rust-code-review")?;
    assert_eq!(g3.name, "rust-code-review");
    assert!(PathBuf::from(&g3.path).ends_with("rust-code-review"));
    assert!(PathBuf::from(&g3.path).join("SKILL.md").is_file());

    // 4. Duplicate & Move scope
    let dup = mgr.duplicate_skill(&g3.id, "rust-code-review-v2", SkillScope::Global, None)?;
    assert_eq!(dup.name, "rust-code-review-v2");
    let moved = mgr.move_skill(&dup.id, SkillScope::Project, Some(&proj.id))?;
    assert_eq!(moved.scope, SkillScope::Project);
    assert_eq!(moved.project_id.as_deref(), Some(proj.id.as_str()));

    // 5. Related files & path traversal rejection
    let sample_script = tmp.0.join("sample.sh");
    fs::write(&sample_script, "#!/bin/sh\necho hi\n")?;
    assert!(mgr
        .add_related_file(&g3.id, "../escape.sh", &sample_script)
        .is_err());
    mgr.add_related_file(&g3.id, "scripts/check.sh", &sample_script)?;
    let related = mgr.list_related_files(&g3.id)?;
    assert_eq!(related.len(), 1);
    assert_eq!(related[0].relative_path, "scripts/check.sh");
    assert!(related[0].is_executable);

    // 6. Export preserves related files and refuses overwrite
    let export_root = tmp.0.join("exports");
    fs::create_dir_all(&export_root)?;
    let exported = mgr.export_skill(&g3.id, &export_root)?;
    assert!(exported.join("SKILL.md").is_file());
    assert!(exported.join("scripts/check.sh").is_file());
    assert!(mgr.export_skill(&g3.id, &export_root).is_err());

    // 7. Delete removes disk directory and DB row
    let dir_path = PathBuf::from(&moved.path);
    mgr.delete_skill(&moved.id)?;
    assert!(!dir_path.exists());
    assert!(mgr.get_skill(&moved.id)?.is_none());

    Ok(())
}

#[test]
fn local_import_single_file_dir_and_multi_dir_with_collision_policies() -> Result<()> {
    let tmp = TempDir::new("import");
    let storage = Arc::new(Storage::open_in_memory()?);
    let mgr = SkillManager::new(tmp.0.join("data"), storage, EventBus::default());

    // Single SKILL.md import
    let single = tmp.0.join("SKILL.md");
    fs::write(
        &single,
        "---\nname: quick-lint\ndescription: Runs quick lint\n---\n\n# Quick Lint\n",
    )?;
    let preview = mgr.inspect_import_source(&single, SkillScope::Global, None)?;
    assert_eq!(preview.candidates.len(), 1);
    assert!(preview.candidates[0].is_single_file);
    assert!(!preview.candidates[0].collides_with_existing);
    let imported = mgr.import_candidates(
        &preview.candidates,
        SkillScope::Global,
        None,
        ImportConflictPolicy::Skip,
        SkillSourceType::Local,
    )?;
    assert_eq!(imported.len(), 1);
    assert_eq!(imported[0].name, "quick-lint");

    // Multi-skill directory with executable script and collision
    let bundle = tmp.0.join("bundle");
    let s1 = bundle.join("quick-lint");
    let s2 = bundle.join("deploy-helper");
    fs::create_dir_all(s1.join("scripts"))?;
    fs::create_dir_all(s2.join("references"))?;
    fs::write(
        s1.join("SKILL.md"),
        "---\nname: quick-lint\ndescription: Duplicate lint\n---\n\n# Body\n",
    )?;
    fs::write(s1.join("scripts/run.sh"), "#!/bin/sh\necho ok\n")?;
    fs::write(
        s2.join("SKILL.md"),
        "---\nname: deploy-helper\ndescription: Helps deploy\n---\n\n# Deploy\n",
    )?;
    fs::write(s2.join("references/guide.md"), "# Guide\n")?;

    let preview2 = mgr.inspect_import_source(&bundle, SkillScope::Global, None)?;
    assert_eq!(preview2.candidates.len(), 2);
    let ql = preview2
        .candidates
        .iter()
        .find(|c| c.name == "quick-lint")
        .unwrap();
    assert!(ql.collides_with_existing);
    assert!(ql.has_executable_scripts);

    // Import with Rename policy: must NOT overwrite existing quick-lint
    let imported2 = mgr.import_candidates(
        &preview2.candidates,
        SkillScope::Global,
        None,
        ImportConflictPolicy::Rename,
        SkillSourceType::Local,
    )?;
    assert_eq!(imported2.len(), 2);
    let names: Vec<String> = mgr.list_skills()?.into_iter().map(|s| s.name).collect();
    assert!(names.contains(&"quick-lint".to_string()));
    assert!(names.contains(&"quick-lint-2".to_string()));
    assert!(names.contains(&"deploy-helper".to_string()));

    Ok(())
}

#[test]
fn project_assignment_and_precedence_end_to_end() -> Result<()> {
    let tmp = TempDir::new("precedence");
    let storage = Arc::new(Storage::open_in_memory()?);
    let ws = storage.ensure_default_workspace()?;
    let p1 = storage.create_project(&ws.id, "P1", "/tmp/p1", "")?;
    let p2 = storage.create_project(&ws.id, "P2", "/tmp/p2", "")?;
    let mgr = SkillManager::new(tmp.0.clone(), storage.clone(), EventBus::default());

    let g1 = mgr.create_skill("shared", "Global shared", SkillScope::Global, None, "G")?;
    let g2 = mgr.create_skill("common", "Global common", SkillScope::Global, None, "C")?;
    let p1_shared = mgr.create_skill(
        "shared",
        "P1 shared",
        SkillScope::Project,
        Some(&p1.id),
        "P1",
    )?;

    // Disable global `common` only in P1
    mgr.set_project_assignment(&g2.id, &p1.id, false)?;
    // Disable P1's `shared` -> must block Global `shared` without fallback
    mgr.set_skill_enabled(&p1_shared.id, false)?;

    let all = mgr.list_skills()?;
    let res_p1 = resolve_skills(Some(&p1.id), &all, &mgr.list_assignments(&p1.id)?, &[]);
    assert!(res_p1.active_skills.is_empty());
    assert!(res_p1
        .conflicts
        .iter()
        .any(|c| c.kind == SkillConflictKind::DisabledProjectBlocksGlobal));
    assert!(res_p1
        .entries
        .iter()
        .any(|e| e.skill.id == g2.id && e.state == EffectiveSkillState::DisabledInProject));

    // P2 is unaffected and gets both Global skills
    let res_p2 = resolve_skills(Some(&p2.id), &all, &mgr.list_assignments(&p2.id)?, &[]);
    let p2_names: Vec<&str> = res_p2.active_skills.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(p2_names, vec!["common", "shared"]);
    assert_eq!(
        res_p2
            .active_skills
            .iter()
            .find(|s| s.name == "shared")
            .unwrap()
            .id,
        g1.id
    );

    Ok(())
}
