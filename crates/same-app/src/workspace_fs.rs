use same_core::paths::simplify_path;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: PathBuf,
    pub rel_path: String,
    pub is_dir: bool,
    pub depth: usize,
}

/// Lists the workspace tree.
///
/// With an empty `filter`, only the children of directories in `expanded`
/// are listed (a collapsible tree). With a filter, every file whose relative
/// path contains it is listed flat, regardless of expansion.
pub fn scan_directory(
    root: &Path,
    filter: &str,
    expanded: &HashSet<PathBuf>,
    max_entries: usize,
) -> Vec<FileEntry> {
    let mut out = Vec::new();
    let filter_lower = filter.trim().to_lowercase();
    if filter_lower.is_empty() {
        walk_tree(root, root, 0, expanded, max_entries, &mut out);
    } else {
        walk_filtered(root, root, 0, &filter_lower, max_entries, &mut out);
    }
    out
}

fn sorted_entries(dir: &Path) -> Vec<(fs::DirEntry, bool)> {
    let Ok(read_dir) = fs::read_dir(dir) else {
        return vec![];
    };
    let mut entries: Vec<(fs::DirEntry, bool, String)> = read_dir
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let name = e.file_name();
            let lossy = name.to_string_lossy();
            if lossy == ".git" || (is_dir && should_skip_dir(&lossy)) {
                return None;
            }
            let lower = lossy.to_lowercase();
            Some((e, is_dir, lower))
        })
        .collect();
    entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.2.cmp(&b.2)));
    entries
        .into_iter()
        .map(|(e, is_dir, _)| (e, is_dir))
        .collect()
}

fn rel_of(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string()
}

fn walk_tree(
    root: &Path,
    dir: &Path,
    depth: usize,
    expanded: &HashSet<PathBuf>,
    max_entries: usize,
    out: &mut Vec<FileEntry>,
) {
    for (entry, is_dir) in sorted_entries(dir) {
        if out.len() >= max_entries {
            return;
        }
        let path = entry.path();
        out.push(FileEntry {
            rel_path: rel_of(root, &path),
            path: path.clone(),
            is_dir,
            depth,
        });
        if is_dir && expanded.contains(&path) {
            walk_tree(root, &path, depth + 1, expanded, max_entries, out);
        }
    }
}

fn walk_filtered(
    root: &Path,
    dir: &Path,
    depth: usize,
    filter_lower: &str,
    max_entries: usize,
    out: &mut Vec<FileEntry>,
) {
    if depth > 12 {
        return;
    }
    for (entry, is_dir) in sorted_entries(dir) {
        if out.len() >= max_entries {
            return;
        }
        let path = entry.path();
        if is_dir {
            walk_filtered(root, &path, depth + 1, filter_lower, max_entries, out);
            continue;
        }
        let rel = rel_of(root, &path);
        if rel.to_lowercase().contains(filter_lower) {
            out.push(FileEntry {
                path,
                rel_path: rel,
                is_dir: false,
                depth: 0,
            });
        }
    }
}

fn should_skip_dir(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | "target"
            | "node_modules"
            | ".next"
            | ".nuxt"
            | ".svelte-kit"
            | ".turbo"
            | ".cache"
            | ".mypy_cache"
            | ".pytest_cache"
            | "dist"
            | "__pycache__"
            | ".venv"
    )
}

#[derive(Debug, Clone, Default)]
pub struct GitInfo {
    pub is_repo: bool,
    pub branch: String,
    pub status: String,
    pub diff: String,
}

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new("git");
    cmd.args(args).current_dir(root);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).to_string())
}

pub fn load_git_info(root: &Path) -> GitInfo {
    // `rev-parse --abbrev-ref HEAD` fails in a fresh repo with no commits,
    // so detect the work tree and the branch separately.
    let inside =
        git(root, &["rev-parse", "--is-inside-work-tree"]).is_some_and(|s| s.trim() == "true");
    if !inside {
        return GitInfo::default();
    }

    let branch = git(root, &["branch", "--show-current"])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            git(root, &["rev-parse", "--short", "HEAD"]).map(|s| format!("(detached {})", s.trim()))
        })
        .unwrap_or_else(|| "(no branch)".to_string());

    let status = git(root, &["status", "--short"]).unwrap_or_default();

    let staged = git(root, &["diff", "--cached"]).unwrap_or_default();
    let unstaged = git(root, &["diff"]).unwrap_or_default();
    let diff = match (staged.trim().is_empty(), unstaged.trim().is_empty()) {
        (true, _) => unstaged,
        (false, true) => staged,
        (false, false) => format!("{staged}\n{unstaged}"),
    };

    GitInfo {
        is_repo: true,
        branch,
        status,
        diff,
    }
}

/// Formats the raw argument passed to `explorer.exe` to select a file in Windows Explorer.
#[cfg_attr(not(any(target_os = "windows", test)), allow(dead_code))]
fn windows_explorer_select_arg(path: &Path) -> String {
    let normalized = simplify_path(&path.to_string_lossy());
    format!("/select,\"{normalized}\"")
}

/// Resolves the directory to open with `xdg-open` on Linux/Unix for a given path.
#[cfg_attr(not(any(all(unix, not(target_os = "macos")), test)), allow(dead_code))]
fn unix_reveal_target(path: &Path, is_dir: bool) -> &Path {
    if is_dir {
        path
    } else {
        path.parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(path)
    }
}

/// Spawns a fire-and-forget launcher process and reaps it in the background on Unix
/// so exited `open` / `xdg-open` processes do not accumulate as zombies.
fn spawn_file_manager_command(mut cmd: Command) {
    #[cfg(unix)]
    {
        if let Ok(mut child) = cmd.spawn() {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
    #[cfg(not(unix))]
    {
        let _ = cmd.spawn();
    }
}

/// Opens a directory in the OS file manager (Windows Explorer / macOS Finder / Linux xdg-open).
pub fn open_path_in_file_manager(path: &Path) {
    #[cfg(target_os = "windows")]
    {
        let normalized = simplify_path(&path.to_string_lossy());
        let mut cmd = Command::new("explorer");
        cmd.arg(normalized);
        spawn_file_manager_command(cmd);
    }
    #[cfg(target_os = "macos")]
    {
        let mut cmd = Command::new("open");
        cmd.arg(path);
        spawn_file_manager_command(cmd);
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let mut cmd = Command::new("xdg-open");
        cmd.arg(path);
        spawn_file_manager_command(cmd);
    }
}

/// Reveals a file or opens a directory in the OS file manager
/// (Windows Explorer `/select`, macOS Finder `open -R`, Linux `xdg-open` on parent dir).
pub fn reveal_in_file_manager(path: &Path, is_dir: bool) {
    if is_dir {
        open_path_in_file_manager(path);
        return;
    }

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        let mut cmd = Command::new("explorer");
        cmd.raw_arg(windows_explorer_select_arg(path));
        spawn_file_manager_command(cmd);
    }
    #[cfg(target_os = "macos")]
    {
        let mut cmd = Command::new("open");
        cmd.arg("-R").arg(path);
        spawn_file_manager_command(cmd);
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let target = unix_reveal_target(path, false);
        let mut cmd = Command::new("xdg-open");
        cmd.arg(target);
        spawn_file_manager_command(cmd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_tree(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("same-fs-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src/nested")).unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::write(root.join("README.md"), "x").unwrap();
        fs::write(root.join("src/main.rs"), "x").unwrap();
        fs::write(root.join("src/nested/deep.rs"), "x").unwrap();
        root
    }

    #[test]
    fn collapsed_tree_lists_top_level_only() {
        let root = tmp_tree("collapsed");
        let entries = scan_directory(&root, "", &HashSet::new(), 100);
        let names: Vec<_> = entries.iter().map(|e| e.rel_path.as_str()).collect();
        assert_eq!(names, vec!["src", "README.md"]);

        let mut expanded = HashSet::new();
        expanded.insert(root.join("src"));
        let entries = scan_directory(&root, "", &expanded, 100);
        let names: Vec<_> = entries.iter().map(|e| e.rel_path.clone()).collect();
        let sep = std::path::MAIN_SEPARATOR;
        assert_eq!(
            names,
            vec![
                "src".to_string(),
                format!("src{sep}nested"),
                format!("src{sep}main.rs"),
                "README.md".to_string()
            ]
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn filter_searches_whole_tree_flat() {
        let root = tmp_tree("filter");
        let entries = scan_directory(&root, "deep", &HashSet::new(), 100);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].rel_path.ends_with("deep.rs"));
        assert_eq!(entries[0].depth, 0);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn windows_explorer_select_arg_handles_spaces_and_verbatim_prefix() {
        assert_eq!(
            windows_explorer_select_arg(Path::new(r"C:\Users\my user\file.rs")),
            r#"/select,"C:\Users\my user\file.rs""#
        );
        assert_eq!(
            windows_explorer_select_arg(Path::new(r"\\?\C:\Users\demo\main.rs")),
            r#"/select,"C:\Users\demo\main.rs""#
        );
        assert_eq!(
            windows_explorer_select_arg(Path::new(r"\\?\UNC\wsl.localhost\Ubuntu\home\main.rs")),
            r#"/select,"\\wsl.localhost\Ubuntu\home\main.rs""#
        );
    }

    #[test]
    fn unix_reveal_target_uses_parent_for_files_and_self_for_dirs() {
        assert_eq!(
            unix_reveal_target(Path::new("/tmp/project/src/main.rs"), false),
            Path::new("/tmp/project/src")
        );
        assert_eq!(
            unix_reveal_target(Path::new("/tmp/project/src"), true),
            Path::new("/tmp/project/src")
        );
        assert_eq!(
            unix_reveal_target(Path::new("main.rs"), false),
            Path::new("main.rs")
        );
    }
}
