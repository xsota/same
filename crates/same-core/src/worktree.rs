use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub is_main: bool,
    pub is_dirty: bool,
}

/// Returns true if `dir` is inside a Git working tree.
pub fn is_git_repo(dir: &Path) -> bool {
    Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(dir)
        .output()
        .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "true")
        .unwrap_or(false)
}

/// Returns the current checked-out branch name, if any.
pub fn current_branch(dir: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Lists local branches in the repository.
pub fn list_branches(dir: &Path) -> Result<Vec<String>> {
    let out = Command::new("git")
        .args(["for-each-ref", "--format=%(refname:short)", "refs/heads"])
        .current_dir(dir)
        .output()
        .context("Failed to run git for-each-ref")?;
    if !out.status.success() {
        bail!(
            "git for-each-ref failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

/// Returns true if the working tree has uncommitted changes (staged, unstaged, or untracked).
pub fn has_uncommitted_changes(dir: &Path) -> Result<bool> {
    let out = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(dir)
        .output()
        .with_context(|| format!("Failed to run git status in {}", dir.display()))?;
    if !out.status.success() {
        bail!(
            "git status failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(!String::from_utf8_lossy(&out.stdout).trim().is_empty())
}

/// Lists all worktrees registered for `repo_dir`.
pub fn list_worktrees(repo_dir: &Path) -> Result<Vec<WorktreeInfo>> {
    if !is_git_repo(repo_dir) {
        return Ok(Vec::new());
    }
    let out = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(repo_dir)
        .output()
        .context("Failed to run git worktree list")?;
    if !out.status.success() {
        bail!(
            "git worktree list failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    let text = String::from_utf8_lossy(&out.stdout);
    let mut result = Vec::new();
    let mut cur_path: Option<PathBuf> = None;
    let mut cur_head: Option<String> = None;
    let mut cur_branch: Option<String> = None;

    let flush = |result: &mut Vec<WorktreeInfo>,
                 cur_path: &mut Option<PathBuf>,
                 cur_head: &mut Option<String>,
                 cur_branch: &mut Option<String>| {
        if let Some(path) = cur_path.take() {
            let is_main = result.is_empty();
            let is_dirty = has_uncommitted_changes(&path).unwrap_or(false);
            result.push(WorktreeInfo {
                path,
                branch: cur_branch.take(),
                head: cur_head.take(),
                is_main,
                is_dirty,
            });
        }
    };

    for line in text.lines() {
        if line.trim().is_empty() {
            flush(&mut result, &mut cur_path, &mut cur_head, &mut cur_branch);
            continue;
        }
        if let Some(p) = line.strip_prefix("worktree ") {
            cur_path = Some(PathBuf::from(p.trim()));
        } else if let Some(h) = line.strip_prefix("HEAD ") {
            cur_head = Some(h.trim().to_string());
        } else if let Some(b) = line.strip_prefix("branch ") {
            let short = b
                .trim()
                .strip_prefix("refs/heads/")
                .unwrap_or(b.trim())
                .to_string();
            cur_branch = Some(short);
        }
    }
    flush(&mut result, &mut cur_path, &mut cur_head, &mut cur_branch);
    Ok(result)
}

/// Picks a branch name starting from `base_slug` that does not collide with
/// any existing local branch or worktree directory.
pub fn unique_worktree_branch(repo_dir: &Path, base_slug: &str) -> String {
    let existing: std::collections::HashSet<String> = list_branches(repo_dir)
        .unwrap_or_default()
        .into_iter()
        .collect();
    if !existing.contains(base_slug) && !default_worktree_path(repo_dir, base_slug).exists() {
        return base_slug.to_string();
    }
    for n in 2..1000 {
        let candidate = format!("{base_slug}-{n}");
        if !existing.contains(&candidate) && !default_worktree_path(repo_dir, &candidate).exists() {
            return candidate;
        }
    }
    format!("{base_slug}-{}", uuid::Uuid::new_v4().simple())
}

/// Resolves the Git repository top-level directory for `dir`, if any.
pub fn repo_root(dir: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(PathBuf::from(s))
    }
}

/// Computes a default path for a new worktree sibling to the Git repository root
/// (even when `repo_dir` is a subdirectory of the repository).
pub fn default_worktree_path(repo_dir: &Path, branch_slug: &str) -> PathBuf {
    let root = repo_root(repo_dir).unwrap_or_else(|| repo_dir.to_path_buf());
    let repo_name = root
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".to_string());
    let sanitized: String = branch_slug
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let parent = root.parent().unwrap_or(&root);
    parent
        .join(".same-worktrees")
        .join(repo_name)
        .join(sanitized)
}

/// Creates a new Git worktree at `worktree_path` on a new branch `branch_name`
/// rooted at `base_branch` (or HEAD if `None`).
pub fn create_worktree(
    repo_dir: &Path,
    worktree_path: &Path,
    branch_name: &str,
    base_branch: Option<&str>,
) -> Result<WorktreeInfo> {
    if !is_git_repo(repo_dir) {
        bail!("Directory is not a Git repository: {}", repo_dir.display());
    }
    if let Some(parent) = worktree_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }

    let mut cmd = Command::new("git");
    cmd.args(["worktree", "add", "-b", branch_name])
        .arg(worktree_path);
    if let Some(base) = base_branch.filter(|b| !b.trim().is_empty()) {
        cmd.arg(base);
    }
    let out = cmd
        .current_dir(repo_dir)
        .output()
        .context("Failed to run git worktree add")?;
    if !out.status.success() {
        bail!(
            "git worktree add failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    let canon = worktree_path
        .canonicalize()
        .unwrap_or_else(|_| worktree_path.to_path_buf());
    Ok(WorktreeInfo {
        path: canon,
        branch: Some(branch_name.to_string()),
        head: None,
        is_main: false,
        is_dirty: false,
    })
}

/// Removes a Git worktree. Refuses to remove a worktree with uncommitted changes
/// unless `force` is explicitly true (never discards uncommitted work automatically).
pub fn remove_worktree(repo_dir: &Path, worktree_path: &Path, force: bool) -> Result<()> {
    if !force && worktree_path.exists() && has_uncommitted_changes(worktree_path)? {
        bail!(
            "Worktree has uncommitted changes; refusing to remove automatically: {}",
            worktree_path.display()
        );
    }
    let mut cmd = Command::new("git");
    cmd.args(["worktree", "remove"]);
    if force {
        cmd.arg("--force");
    }
    cmd.arg(worktree_path);
    let out = cmd
        .current_dir(repo_dir)
        .output()
        .context("Failed to run git worktree remove")?;
    if !out.status.success() {
        bail!(
            "git worktree remove failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Merges `branch_name` into the currently checked-out branch of `repo_dir`.
/// Must only be called after explicit user confirmation. Refuses if `repo_dir`
/// itself has uncommitted changes.
pub fn merge_worktree_branch(repo_dir: &Path, branch_name: &str) -> Result<String> {
    if !is_git_repo(repo_dir) {
        bail!("Directory is not a Git repository: {}", repo_dir.display());
    }
    if has_uncommitted_changes(repo_dir)? {
        bail!("Main working tree has uncommitted changes; commit or stash them before merging.");
    }
    let out = Command::new("git")
        .args(["merge", "--no-ff", branch_name])
        .current_dir(repo_dir)
        .output()
        .context("Failed to run git merge")?;
    if !out.status.success() {
        bail!(
            "git merge failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use uuid::Uuid;

    fn init_temp_repo() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("same-wt-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(&dir)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["init"]);
        run(&["config", "user.email", "same@example.com"]);
        run(&["config", "user.name", "SAME Test"]);
        fs::write(dir.join("README.md"), "hello\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "initial"]);
        dir
    }

    #[test]
    fn non_git_dir_is_handled_gracefully() {
        let dir = std::env::temp_dir().join(format!("same-nongit-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        assert!(!is_git_repo(&dir));
        assert!(list_worktrees(&dir).unwrap().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn worktree_lifecycle_protects_uncommitted_changes() {
        let repo = init_temp_repo();
        let wt_path = repo
            .parent()
            .unwrap()
            .join(format!("wt-{}", Uuid::new_v4()));

        let info = create_worktree(&repo, &wt_path, "feat/parallel-test", None).unwrap();
        assert_eq!(info.branch.as_deref(), Some("feat/parallel-test"));

        let listed = list_worktrees(&repo).unwrap();
        assert!(listed.len() >= 2);

        // Dirty worktree cannot be removed without force.
        fs::write(wt_path.join("dirty.txt"), "unsaved work").unwrap();
        assert!(has_uncommitted_changes(&wt_path).unwrap());
        assert!(remove_worktree(&repo, &wt_path, false).is_err());

        // Clean up the uncommitted file -> removal succeeds.
        fs::remove_file(wt_path.join("dirty.txt")).unwrap();
        remove_worktree(&repo, &wt_path, false).unwrap();

        let _ = fs::remove_dir_all(&repo);
    }
}
