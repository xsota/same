use same_acp::exec::ExecTarget;
use same_core::skills::validator::validate_skill_dir;
use same_core::{ExternalSkill, SkillOrigin};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use tokio::process::Command;

fn wsl_home_cache() -> &'static Mutex<HashMap<String, Option<String>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Option<String>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Resolves the user's home directory on `target` as a host-accessible `PathBuf`.
pub async fn target_home_dir(target: &ExecTarget) -> Option<PathBuf> {
    match target {
        ExecTarget::Host => directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf()),
        ExecTarget::Wsl { distro } => {
            let linux_home = wsl_linux_home(distro).await?;
            Some(target.to_host_path(Path::new("/"), &linux_home))
        }
    }
}

async fn wsl_linux_home(distro: &str) -> Option<String> {
    if let Some(cached) = wsl_home_cache().lock().unwrap().get(distro) {
        return cached.clone();
    }
    let mut cmd = Command::new("wsl.exe");
    cmd.args(["-d", distro, "--exec", "sh", "-c", r#"printf %s "$HOME""#])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);
    let home = match tokio::time::timeout(std::time::Duration::from_secs(5), cmd.output()).await {
        Ok(Ok(out)) if out.status.success() => {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            (!s.is_empty()).then_some(s)
        }
        _ => None,
    };
    wsl_home_cache()
        .lock()
        .unwrap()
        .insert(distro.to_string(), home.clone());
    home
}

struct DiscoveryRoot {
    dir: PathBuf,
    origin: SkillOrigin,
    provider_id: &'static str,
    source_label: String,
    /// Scan `.system/` or nested built-in folders one level deeper when true.
    scan_system_subdir: bool,
}

/// Discovers external and built-in skills from known agent and project locations
/// on `target` (read-only; never modifies or deletes any files).
pub async fn discover_external_skills(
    target: &ExecTarget,
    project_cwd: Option<&Path>,
) -> Vec<ExternalSkill> {
    let mut roots: Vec<DiscoveryRoot> = Vec::new();
    let target_label = target.label();

    if let Some(cwd) = project_cwd {
        roots.push(DiscoveryRoot {
            dir: cwd.join(".agents").join("skills"),
            origin: SkillOrigin::External,
            provider_id: "shared",
            source_label: ".agents/skills".to_string(),
            scan_system_subdir: false,
        });
        roots.push(DiscoveryRoot {
            dir: cwd.join(".agent").join("skills"),
            origin: SkillOrigin::External,
            provider_id: "antigravity",
            source_label: ".agent/skills".to_string(),
            scan_system_subdir: false,
        });
        roots.push(DiscoveryRoot {
            dir: cwd.join(".claude").join("skills"),
            origin: SkillOrigin::External,
            provider_id: "claude",
            source_label: ".claude/skills".to_string(),
            scan_system_subdir: false,
        });
        roots.push(DiscoveryRoot {
            dir: cwd.join(".codex").join("skills"),
            origin: SkillOrigin::External,
            provider_id: "codex",
            source_label: ".codex/skills".to_string(),
            scan_system_subdir: false,
        });
        roots.push(DiscoveryRoot {
            dir: cwd.join(".opencode").join("skills"),
            origin: SkillOrigin::External,
            provider_id: "opencode",
            source_label: ".opencode/skills".to_string(),
            scan_system_subdir: false,
        });
    }

    if let Some(home) = target_home_dir(target).await {
        roots.push(DiscoveryRoot {
            dir: home.join(".agents").join("skills"),
            origin: SkillOrigin::External,
            provider_id: "shared",
            source_label: "~/.agents/skills".to_string(),
            scan_system_subdir: false,
        });
        roots.push(DiscoveryRoot {
            dir: home.join(".claude").join("skills"),
            origin: SkillOrigin::External,
            provider_id: "claude",
            source_label: "~/.claude/skills".to_string(),
            scan_system_subdir: false,
        });
        roots.push(DiscoveryRoot {
            dir: home.join(".codex").join("skills"),
            origin: SkillOrigin::External,
            provider_id: "codex",
            source_label: "~/.codex/skills".to_string(),
            scan_system_subdir: true,
        });
        roots.push(DiscoveryRoot {
            dir: home.join(".config").join("opencode").join("skills"),
            origin: SkillOrigin::External,
            provider_id: "opencode",
            source_label: "~/.config/opencode/skills".to_string(),
            scan_system_subdir: false,
        });
        roots.push(DiscoveryRoot {
            dir: home.join(".gemini").join("config").join("skills"),
            origin: SkillOrigin::External,
            provider_id: "antigravity",
            source_label: "~/.gemini/config/skills".to_string(),
            scan_system_subdir: false,
        });
        roots.push(DiscoveryRoot {
            dir: home
                .join(".gemini")
                .join("antigravity-cli")
                .join("builtin")
                .join("skills"),
            origin: SkillOrigin::BuiltIn,
            provider_id: "antigravity",
            source_label: "~/.gemini/antigravity-cli/builtin/skills".to_string(),
            scan_system_subdir: false,
        });
    }

    let mut seen_paths: HashSet<PathBuf> = HashSet::new();
    let mut found: Vec<ExternalSkill> = Vec::new();

    for root in roots {
        scan_root(&root, &target_label, &mut seen_paths, &mut found);
    }

    found.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then(a.source_label.cmp(&b.source_label))
    });
    found
}

fn scan_root(
    root: &DiscoveryRoot,
    target_label: &str,
    seen_paths: &mut HashSet<PathBuf>,
    out: &mut Vec<ExternalSkill>,
) {
    if !root.dir.is_dir() {
        return;
    }
    let Ok(entries) = fs::read_dir(&root.dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name_os = entry.file_name();
        let name_str = name_os.to_string_lossy();
        if !path.is_dir() {
            continue;
        }
        // Skip trash/hidden dirs except Codex's `.system` built-in skills.
        if name_str.starts_with('.') {
            if root.scan_system_subdir && name_str == ".system" {
                let sys_root = DiscoveryRoot {
                    dir: path,
                    origin: SkillOrigin::BuiltIn,
                    provider_id: root.provider_id,
                    source_label: format!("{}/.system", root.source_label),
                    scan_system_subdir: false,
                };
                scan_root(&sys_root, target_label, seen_paths, out);
            }
            continue;
        }
        if !path.join("SKILL.md").is_file() {
            continue;
        }
        let canon = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if !seen_paths.insert(canon) {
            continue;
        }
        let (parsed, related, _) = validate_skill_dir(&path);
        let skill_name = parsed
            .as_ref()
            .map(|p| p.name.clone())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| name_str.to_string());
        let description = parsed
            .as_ref()
            .map(|p| p.description.clone())
            .unwrap_or_default();
        let has_scripts = related.iter().any(|f| f.is_executable);
        out.push(ExternalSkill {
            name: skill_name,
            description,
            origin: root.origin,
            provider_id: root.provider_id.to_string(),
            source_label: root.source_label.clone(),
            path: path.to_string_lossy().to_string(),
            execution_target: target_label.to_string(),
            has_scripts,
            related_files: related,
        });
    }
}
