use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let brand = manifest_dir.join("../../assets/brand");
    println!("cargo:rerun-if-changed=build.rs");

    emit_build_info(&manifest_dir);
    println!(
        "cargo:rerun-if-changed={}",
        brand.join("same.ico").display()
    );

    // Windows: embed the app icon as resource 1, which GPUI loads for the
    // window and taskbar (and Explorer shows for same.exe).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let ico = brand
            .join("same.ico")
            .canonicalize()
            .expect("assets/brand/same.ico");
        // Written with an absolute path: the resource compiler resolves
        // relative paths against the preprocessed copy, not this crate.
        let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("same.rc");
        let ico = ico.to_string_lossy().replace('\\', "\\\\");
        std::fs::write(&out, format!("1 ICON \"{ico}\"\n")).unwrap();
        embed_resource::compile(&out, embed_resource::NONE)
            .manifest_optional()
            .unwrap();
        // Match macOS/Linux's 8 MiB main-thread stack on Windows MSVC (default
        // is only 1 MiB, which unoptimized GPUI debug views can overflow).
        if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
            println!("cargo:rustc-link-arg=/STACK:8388608");
        }
    }
}

/// Exposes `SAME_GIT_SHA` (short commit hash, "unknown" without git) to the
/// crate; debug builds show it next to the version.
fn emit_build_info(manifest_dir: &std::path::Path) {
    let git_out = |args: &[&str]| -> Option<String> {
        std::process::Command::new("git")
            .args(args)
            .current_dir(manifest_dir)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
    };

    // Rebuild when HEAD or the active branch ref moves (works in linked
    // worktrees and checkouts without reflogs).
    let watch_git_path = |rel: &str| {
        if let Some(p) = git_out(&["rev-parse", "--git-path", rel]) {
            let full = manifest_dir.join(p);
            if full.exists() {
                println!("cargo:rerun-if-changed={}", full.display());
            }
        }
    };
    watch_git_path("HEAD");
    if let Some(symref) = git_out(&["symbolic-ref", "-q", "HEAD"]) {
        watch_git_path(&symref);
    }
    watch_git_path("packed-refs");

    let sha = git_out(&["rev-parse", "--short=7", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=SAME_GIT_SHA={sha}");
}
