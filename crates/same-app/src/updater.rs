use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::cmp::Ordering;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

/// Executable path captured before `self_replace` renames over it (on Linux,
/// `/proc/self/exe` gains a ` (deleted)` suffix after replacement).
static ORIGINAL_EXE: OnceLock<PathBuf> = OnceLock::new();

pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const GITHUB_REPO: &str = "xsota/same";

/// True for `--release` builds (what CI ships).
pub const IS_RELEASE_BUILD: bool = !cfg!(debug_assertions);
/// Short commit hash the binary was built from ("unknown" without git).
pub const GIT_SHA: &str = env!("SAME_GIT_SHA");

/// The version shown to users: `v0.1.0` for release builds, and
/// `v0.1.0-dev (abc1234)` for debug builds.
pub fn display_version() -> String {
    if IS_RELEASE_BUILD {
        format!("v{CURRENT_VERSION}")
    } else {
        format!("v{CURRENT_VERSION}-dev ({GIT_SHA})")
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ReleaseInfo {
    pub version: String,
    pub tag: String,
    pub html_url: String,
    pub notes: String,
    pub download_url: Option<String>,
    pub asset_name: Option<String>,
    /// URL of the release's `SHA256SUMS.txt` asset.
    pub checksums_url: Option<String>,
}

/// Name of the checksum manifest the release workflow publishes.
const CHECKSUMS_ASSET_NAME: &str = "SHA256SUMS.txt";

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum UpdateCheckOutcome {
    UpToDate { current: String, latest: String },
    Available(ReleaseInfo),
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    assets: Vec<GithubAsset>,
}

#[derive(Debug, Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
}

pub fn expected_asset_suffix() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("linux-x86_64.tar.gz"),
        ("windows", "x86_64") => Some("windows-x86_64.zip"),
        ("macos", "aarch64") => Some("macos-aarch64.tar.gz"),
        ("macos", "x86_64") => Some("macos-x86_64.tar.gz"),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SemVer {
    major: u64,
    minor: u64,
    patch: u64,
    /// Empty means a stable release (higher precedence than any prerelease).
    prerelease: Vec<String>,
}

fn parse_semver(v: &str) -> Option<SemVer> {
    let trimmed = v.trim().trim_start_matches('v').trim_start_matches('V');
    let without_build = trimmed.split('+').next()?;
    let (core, pre) = match without_build.split_once('-') {
        Some((c, p)) => (c, Some(p)),
        None => (without_build, None),
    };
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    let prerelease = match pre {
        Some(p) if !p.is_empty() => p.split('.').map(str::to_string).collect(),
        _ => Vec::new(),
    };
    Some(SemVer {
        major,
        minor,
        patch,
        prerelease,
    })
}

impl Ord for SemVer {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (self.prerelease.is_empty(), other.prerelease.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => {
                    for (a, b) in self.prerelease.iter().zip(other.prerelease.iter()) {
                        let ord = match (a.parse::<u64>(), b.parse::<u64>()) {
                            (Ok(na), Ok(nb)) => na.cmp(&nb),
                            (Ok(_), Err(_)) => Ordering::Less,
                            (Err(_), Ok(_)) => Ordering::Greater,
                            (Err(_), Err(_)) => a.cmp(b),
                        };
                        if ord != Ordering::Equal {
                            return ord;
                        }
                    }
                    self.prerelease.len().cmp(&other.prerelease.len())
                }
            })
    }
}

impl PartialOrd for SemVer {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

pub fn is_newer_version(latest: &str, current: &str) -> bool {
    match (parse_semver(latest), parse_semver(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

fn build_http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(format!("same/{CURRENT_VERSION}"))
        .timeout(Duration::from_secs(30))
        .build()
        .context("Failed to create HTTP client")
}

pub async fn check_for_updates() -> Result<UpdateCheckOutcome> {
    let client = build_http_client()?;
    let url = format!("https://api.github.com/repos/{GITHUB_REPO}/releases/latest");
    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .context("Failed to reach GitHub Releases API")?;

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(UpdateCheckOutcome::UpToDate {
            current: CURRENT_VERSION.to_string(),
            latest: CURRENT_VERSION.to_string(),
        });
    }

    if !resp.status().is_success() {
        bail!("GitHub API returned HTTP {}", resp.status());
    }

    let release: GithubRelease = resp
        .json()
        .await
        .context("Failed to parse GitHub release response")?;

    let latest_version = release
        .tag_name
        .trim()
        .trim_start_matches('v')
        .trim_start_matches('V')
        .to_string();

    if !is_newer_version(&latest_version, CURRENT_VERSION) {
        return Ok(UpdateCheckOutcome::UpToDate {
            current: CURRENT_VERSION.to_string(),
            latest: latest_version,
        });
    }

    let suffix = expected_asset_suffix();
    let matched_asset = suffix.and_then(|suf| {
        release
            .assets
            .iter()
            .find(|a| a.name.ends_with(suf))
    });
    let checksums_asset = release
        .assets
        .iter()
        .find(|a| a.name == CHECKSUMS_ASSET_NAME);

    Ok(UpdateCheckOutcome::Available(ReleaseInfo {
        version: latest_version,
        tag: release.tag_name,
        html_url: release.html_url,
        notes: release.body.unwrap_or_default(),
        download_url: matched_asset.map(|a| a.browser_download_url.clone()),
        asset_name: matched_asset.map(|a| a.name.clone()),
        checksums_url: checksums_asset.map(|a| a.browser_download_url.clone()),
    }))
}

pub async fn download_and_install(release: &ReleaseInfo) -> Result<()> {
    let download_url = release
        .download_url
        .as_deref()
        .ok_or_else(|| anyhow!("No release archive found for this platform"))?;
    let asset_name = release
        .asset_name
        .as_deref()
        .ok_or_else(|| anyhow!("No release archive found for this platform"))?;
    // Refuse to install anything we cannot verify.
    let checksums_url = release
        .checksums_url
        .as_deref()
        .ok_or_else(|| anyhow!("Release has no {CHECKSUMS_ASSET_NAME}; refusing to install unverified update"))?;

    let client = reqwest::Client::builder()
        .user_agent(format!("same/{CURRENT_VERSION}"))
        .timeout(Duration::from_secs(300))
        .build()
        .context("Failed to create HTTP client")?;

    let sums_resp = client
        .get(checksums_url)
        .send()
        .await
        .context("Failed to download checksum manifest")?;
    if !sums_resp.status().is_success() {
        bail!("Checksum manifest download failed with HTTP {}", sums_resp.status());
    }
    let sums_text = sums_resp
        .text()
        .await
        .context("Failed to read checksum manifest")?;
    let expected_sha256 = find_checksum(&sums_text, asset_name)
        .ok_or_else(|| anyhow!("{asset_name} is not listed in {CHECKSUMS_ASSET_NAME}"))?;

    let resp = client
        .get(download_url)
        .send()
        .await
        .context("Failed to download update archive")?;

    if !resp.status().is_success() {
        bail!("Download failed with HTTP {}", resp.status());
    }

    let bytes = resp
        .bytes()
        .await
        .context("Failed to read update archive body")?
        .to_vec();

    let actual_sha256 = sha256_hex(&bytes);
    if !actual_sha256.eq_ignore_ascii_case(&expected_sha256) {
        bail!(
            "Checksum mismatch for {asset_name}: expected {expected_sha256}, got {actual_sha256}"
        );
    }

    let asset_name = asset_name.to_string();
    tokio::task::spawn_blocking(move || extract_and_replace(&bytes, &asset_name))
        .await
        .context("Update task panicked")??;

    Ok(())
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Looks up `asset_name` in `sha256sum` output (`<hex>  <name>`, or
/// `<hex> *<name>` in binary mode) and returns its hex digest.
fn find_checksum(manifest: &str, asset_name: &str) -> Option<String> {
    manifest.lines().find_map(|line| {
        let (hash, name) = line.trim().split_once(char::is_whitespace)?;
        let name = name.trim_start().trim_start_matches('*');
        let valid_hash = hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit());
        (valid_hash && name == asset_name).then(|| hash.to_ascii_lowercase())
    })
}

fn expected_binary_name() -> &'static str {
    if cfg!(windows) {
        "same.exe"
    } else {
        "same"
    }
}

fn extract_and_replace(archive_bytes: &[u8], asset_name: &str) -> Result<()> {
    let exe_path = std::env::current_exe().ok();
    // Stage on the same filesystem as the installed binary when possible so
    // the compatibility probe works even if /tmp is mounted `noexec`.
    let staging_base = exe_path
        .as_deref()
        .and_then(|p| p.parent())
        .map(Path::to_path_buf)
        .unwrap_or_else(std::env::temp_dir);
    let tmp_dir = staging_base.join(format!(".same-update-{}", uuid::Uuid::new_v4()));
    let tmp_dir = if fs::create_dir_all(&tmp_dir).is_ok() {
        tmp_dir
    } else {
        let fallback = std::env::temp_dir().join(format!("same-update-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&fallback).context("Failed to create temporary update directory")?;
        fallback
    };

    let result = (|| -> Result<()> {
        let bin_path = if asset_name.ends_with(".zip") {
            extract_zip_binary(archive_bytes, &tmp_dir)?
        } else if asset_name.ends_with(".tar.gz") || asset_name.ends_with(".tgz") {
            extract_tar_gz_binary(archive_bytes, &tmp_dir)?
        } else {
            bail!("Unsupported archive format: {asset_name}");
        };

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&bin_path)?.permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&bin_path, perms)?;
        }

        // Verify the downloaded binary actually executes on this system
        // (catches glibc / loader mismatches before replacing the working app).
        let probe = std::process::Command::new(&bin_path)
            .arg("--version")
            .output()
            .context("Downloaded binary could not be executed on this system")?;
        if !probe.status.success() {
            let stderr = String::from_utf8_lossy(&probe.stderr);
            bail!(
                "Downloaded binary failed compatibility check (exit {:?}): {}",
                probe.status.code(),
                stderr.trim()
            );
        }

        if let Ok(exe) = std::env::current_exe() {
            let _ = ORIGINAL_EXE.set(exe);
        }

        self_replace::self_replace(&bin_path)
            .context("Failed to replace current executable")?;
        Ok(())
    })();

    let _ = fs::remove_dir_all(&tmp_dir);
    result
}

fn extract_tar_gz_binary(archive_bytes: &[u8], dest_dir: &Path) -> Result<PathBuf> {
    let gz = flate2::read::GzDecoder::new(Cursor::new(archive_bytes));
    let mut archive = tar::Archive::new(gz);
    let target_name = expected_binary_name();

    for entry in archive.entries().context("Failed to read tar entries")? {
        let mut entry = entry.context("Failed to read tar entry")?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path().context("Invalid path in tar archive")?;
        if path.file_name().and_then(|n| n.to_str()) == Some(target_name) {
            let out_path = dest_dir.join(target_name);
            entry
                .unpack(&out_path)
                .context("Failed to extract binary from tar.gz")?;
            return Ok(out_path);
        }
    }

    bail!("Binary '{target_name}' not found in archive")
}

fn extract_zip_binary(archive_bytes: &[u8], dest_dir: &Path) -> Result<PathBuf> {
    let mut archive =
        zip::ZipArchive::new(Cursor::new(archive_bytes)).context("Failed to open zip archive")?;
    let target_name = expected_binary_name();

    for i in 0..archive.len() {
        let mut file = archive.by_index(i).context("Failed to read zip entry")?;
        if !file.is_file() {
            continue;
        }
        let name = file.name().to_string();
        if Path::new(&name).file_name().and_then(|n| n.to_str()) == Some(target_name) {
            let out_path = dest_dir.join(target_name);
            let mut out_file =
                fs::File::create(&out_path).context("Failed to create extracted binary file")?;
            std::io::copy(&mut file, &mut out_file)
                .context("Failed to extract binary from zip")?;
            return Ok(out_path);
        }
    }

    bail!("Binary '{target_name}' not found in zip archive")
}

pub fn restart_self() -> Result<()> {
    let exe = match ORIGINAL_EXE.get() {
        Some(path) => path.clone(),
        None => std::env::current_exe().context("Failed to locate current executable")?,
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::Command::new(exe)
        .args(args)
        .spawn()
        .context("Failed to relaunch SAME")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_semver_versions() {
        assert!(is_newer_version("0.2.0", "0.1.0"));
        assert!(is_newer_version("v0.1.1", "0.1.0"));
        assert!(is_newer_version("1.0.0", "0.9.9"));
        assert!(!is_newer_version("0.1.0", "0.1.0"));
        assert!(!is_newer_version("0.0.9", "0.1.0"));
        assert!(is_newer_version("0.2.0-rc.1", "0.1.0"));
        assert!(is_newer_version("0.2.0", "0.2.0-rc.1"));
        assert!(!is_newer_version("0.2.0-rc.1", "0.2.0"));
        assert!(is_newer_version("0.2.0-rc.2", "0.2.0-rc.1"));
        assert!(is_newer_version("0.2.0-rc.10", "0.2.0-rc.2"));
    }

    #[test]
    fn sha256_hex_matches_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn finds_checksum_in_sha256sum_output() {
        let a = "a".repeat(64);
        let b = "B".repeat(64);
        let manifest = format!(
            "{a}  same-linux-x86_64.tar.gz\n{b} *same-windows-x86_64.zip\n"
        );
        assert_eq!(
            find_checksum(&manifest, "same-linux-x86_64.tar.gz"),
            Some(a.clone())
        );
        assert_eq!(
            find_checksum(&manifest, "same-windows-x86_64.zip"),
            Some(b.to_ascii_lowercase())
        );
        assert_eq!(find_checksum(&manifest, "same-macos-aarch64.tar.gz"), None);
        // A suffix of a listed name must not match.
        assert_eq!(find_checksum(&manifest, "x86_64.tar.gz"), None);
        // Malformed hashes are ignored.
        assert_eq!(find_checksum("deadbeef  same-linux-x86_64.tar.gz", "same-linux-x86_64.tar.gz"), None);
    }
}
