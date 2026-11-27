//! Where agent processes run.
//!
//! On Windows a project can live inside a WSL distribution
//! (`\\wsl.localhost\<distro>\home\...`). Agents for such a project must run
//! inside that distribution: that is where the user installed and logged in
//! to their CLIs, and where the project's toolchain lives. This module detects
//! the target from the project path and builds the matching command.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use tokio::process::Command;

/// How the user wants a project's agents to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExecPreference {
    /// WSL when the project path is inside a WSL distribution, else native.
    #[default]
    Auto,
    /// Always the host OS (Windows binaries on Windows).
    Host,
    /// Always WSL (the distribution of the project path, or the default one).
    Wsl,
}

/// Where a process actually runs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ExecTarget {
    #[default]
    Host,
    Wsl {
        distro: String,
    },
}

impl ExecTarget {
    pub fn label(&self) -> String {
        match self {
            ExecTarget::Host => {
                if cfg!(windows) {
                    "Windows".to_string()
                } else {
                    "Local".to_string()
                }
            }
            ExecTarget::Wsl { distro } => format!("WSL: {distro}"),
        }
    }

    /// Converts a host path (as stored for the project) into the path the
    /// target sees. Unchanged for [`ExecTarget::Host`].
    pub fn to_target_path(&self, host_path: &Path) -> String {
        let s = host_path.to_string_lossy().to_string();
        match self {
            ExecTarget::Host => s,
            ExecTarget::Wsl { distro } => {
                if let Some((d, linux)) = wsl_location(&s) {
                    if d.eq_ignore_ascii_case(distro) {
                        return linux;
                    }
                }
                windows_to_mnt(&s).unwrap_or(s)
            }
        }
    }

    /// Converts a path an agent sent (in the target's terms) back to a path
    /// this process can open. Relative paths resolve against `base_cwd`.
    pub fn to_host_path(&self, base_cwd: &Path, raw: &str) -> PathBuf {
        if let ExecTarget::Wsl { distro } = self {
            if raw.starts_with('/') {
                if let Some(win) = mnt_to_windows(raw) {
                    return PathBuf::from(win);
                }
                return PathBuf::from(format!(
                    r"\\wsl.localhost\{distro}{}",
                    raw.replace('/', "\\")
                ));
            }
        }
        let p = Path::new(raw);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            base_cwd.join(p)
        }
    }
}

/// Splits `\\wsl.localhost\<distro>\a\b` (or `\\wsl$\...`) into the distro
/// name and the Linux path `/a/b`.
pub fn wsl_location(path: &str) -> Option<(String, String)> {
    let p = path.replace('/', "\\");
    let p = p
        .strip_prefix(r"\\?\UNC\")
        .map(|r| format!(r"\\{r}"))
        .unwrap_or(p);
    let lower = p.to_ascii_lowercase();
    let rest = if lower.starts_with(r"\\wsl.localhost\") {
        &p[r"\\wsl.localhost\".len()..]
    } else if lower.starts_with(r"\\wsl$\") {
        &p[r"\\wsl$\".len()..]
    } else {
        return None;
    };
    let mut parts = rest.splitn(2, '\\');
    let distro = parts.next().filter(|d| !d.is_empty())?.to_string();
    let tail = parts.next().unwrap_or("").trim_end_matches('\\');
    let linux = if tail.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", tail.replace('\\', "/"))
    };
    Some((distro, linux))
}

/// `C:\x\y` -> `/mnt/c/x/y`.
fn windows_to_mnt(path: &str) -> Option<String> {
    let b = path.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        let drive = (b[0] as char).to_ascii_lowercase();
        let rest = path[2..].replace('\\', "/");
        let rest = rest.trim_start_matches('/');
        return Some(if rest.is_empty() {
            format!("/mnt/{drive}")
        } else {
            format!("/mnt/{drive}/{rest}")
        });
    }
    None
}

/// `/mnt/c/x/y` -> `C:\x\y`.
fn mnt_to_windows(path: &str) -> Option<String> {
    let rest = path.strip_prefix("/mnt/")?;
    let mut parts = rest.splitn(2, '/');
    let drive = parts.next()?;
    if drive.len() != 1 || !drive.as_bytes()[0].is_ascii_alphabetic() {
        return None;
    }
    let tail = parts.next().unwrap_or("").replace('/', "\\");
    Some(format!("{}:\\{}", drive.to_ascii_uppercase(), tail))
}

/// Decides where to run agents for a project at `cwd` (a host path).
/// Off Windows there is no WSL and this is always [`ExecTarget::Host`].
pub async fn resolve_target(pref: ExecPreference, cwd: &Path) -> ExecTarget {
    if !cfg!(windows) {
        return ExecTarget::Host;
    }
    let detected = wsl_location(&cwd.to_string_lossy()).map(|(d, _)| d);
    match (pref, detected) {
        (ExecPreference::Host, _) => ExecTarget::Host,
        (_, Some(distro)) => ExecTarget::Wsl { distro },
        (ExecPreference::Auto, None) => ExecTarget::Host,
        (ExecPreference::Wsl, None) => match default_distro().await {
            Some(distro) => ExecTarget::Wsl { distro },
            None => ExecTarget::Host,
        },
    }
}

/// The target `resolve_target` would pick without asking WSL anything, for
/// display. `None` means "WSL, default distribution".
pub fn predicted_target(pref: ExecPreference, cwd: &Path) -> Option<ExecTarget> {
    if !cfg!(windows) {
        return Some(ExecTarget::Host);
    }
    let detected = wsl_location(&cwd.to_string_lossy()).map(|(d, _)| d);
    match (pref, detected) {
        (ExecPreference::Host, _) => Some(ExecTarget::Host),
        (_, Some(distro)) => Some(ExecTarget::Wsl { distro }),
        (ExecPreference::Auto, None) => Some(ExecTarget::Host),
        (ExecPreference::Wsl, None) => None,
    }
}

fn wsl_command() -> Command {
    let mut cmd = Command::new("wsl.exe");
    no_window(&mut cmd);
    cmd
}

fn no_window(cmd: &mut Command) {
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    #[cfg(not(windows))]
    let _ = cmd;
}

async fn default_distro() -> Option<String> {
    let out = wsl_command()
        .args(["--exec", "printenv", "WSL_DISTRO_NAME"])
        .stdin(Stdio::null())
        .output()
        .await
        .ok()?;
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !name.is_empty()).then_some(name)
}

/// What the user's login + interactive shell in a distribution sets up.
#[derive(Debug, Clone, Default)]
struct LoginEnv {
    path: String,
    /// Variables the shell's rc files add or change (e.g. API keys exported
    /// in `~/.zshenv`), compared with a bare `wsl --exec`.
    vars: Vec<(String, String)>,
}

fn path_cache() -> &'static Mutex<HashMap<String, Option<LoginEnv>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Option<LoginEnv>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

const ENV_MARKER: &str = "__SAME_ENV__";

/// Variables never copied from the login shell: per-process shell state,
/// and ones `wsl.exe` or `env` set themselves.
fn forwardable(name: &str) -> bool {
    const SKIP: &[&str] = &[
        "PATH", "PWD", "OLDPWD", "SHLVL", "_", "WSLENV", "TERM", "COLUMNS", "LINES",
    ];
    !SKIP.contains(&name)
        && !name.starts_with("WSL_")
        && !name.is_empty()
        && !name.as_bytes()[0].is_ascii_digit()
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Parses `env -0` output into pairs.
fn parse_env0(bytes: &[u8]) -> Vec<(String, String)> {
    String::from_utf8_lossy(bytes)
        .split('\0')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.trim_start_matches('\n').to_string(), v.to_string()))
        .collect()
}

/// The PATH and environment of the user's login + interactive shell in
/// `distro`, so tools installed via `~/.zshrc` (volta, nvm, ~/.local/bin ...)
/// are found, and variables such as API keys are set, the same way they are
/// in the user's terminal. Cached per distribution.
async fn login_env(distro: &str) -> Option<LoginEnv> {
    if let Some(cached) = path_cache().lock().unwrap().get(distro) {
        return cached.clone();
    }
    // The shell may print banners; only what follows the marker is read.
    let script = r#"s=$(getent passwd "$(id -un)" | cut -d: -f7); exec "${s:-/bin/sh}" -lic "$1" </dev/null"#;
    let probe = format!(r#"printf '\n{ENV_MARKER}\n'; env -0"#);
    let shell = wsl_command()
        .args(["-d", distro, "--exec", "sh", "-c", script, "_", &probe])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let bare = wsl_command()
        .args(["-d", distro, "--exec", "env", "-0"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let timeout = std::time::Duration::from_secs(20);
    let (shell, bare) = tokio::join!(
        tokio::time::timeout(timeout, shell),
        tokio::time::timeout(timeout, bare)
    );
    let env = match shell {
        Ok(Ok(out)) => {
            let marker = format!("\n{ENV_MARKER}\n");
            let pos = out
                .stdout
                .windows(marker.len())
                .position(|w| w == marker.as_bytes());
            pos.map(|p| parse_env0(&out.stdout[p + marker.len()..]))
        }
        _ => None,
    };
    let login = env.and_then(|vars| {
        let path = vars
            .iter()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| v.clone())
            .filter(|p| !p.is_empty())?;
        let bare: HashMap<String, String> = match bare {
            Ok(Ok(out)) => parse_env0(&out.stdout).into_iter().collect(),
            _ => HashMap::new(),
        };
        let vars = vars
            .into_iter()
            .filter(|(k, v)| forwardable(k) && bare.get(k) != Some(v))
            .collect();
        Some(LoginEnv { path, vars })
    });
    path_cache()
        .lock()
        .unwrap()
        .insert(distro.to_string(), login.clone());
    login
}

/// Builds a command that runs `program args` in `target` with working
/// directory `cwd` (a host path). Environment variables in `env` reach the
/// program in both cases.
pub async fn build_command(
    target: &ExecTarget,
    program: &str,
    args: &[String],
    cwd: &Path,
    env: &BTreeMap<String, String>,
) -> Command {
    match target {
        ExecTarget::Host => {
            let mut cmd = Command::new(program);
            cmd.args(args).current_dir(cwd);
            if cfg!(unix) && !env.contains_key("PATH") {
                if let Some(login_path) = host_login_path().await {
                    let inherited = std::env::var("PATH").unwrap_or_default();
                    let merged = merge_unix_paths(&inherited, &login_path);
                    cmd.env("PATH", merged);
                }
            }
            for (k, v) in env {
                cmd.env(k, v);
            }
            no_window(&mut cmd);
            cmd
        }
        ExecTarget::Wsl { distro } => {
            let linux_cwd = target.to_target_path(cwd);
            build_wsl_command(distro, program, args, Some(&linux_cwd), env).await
        }
    }
}

/// Like [`build_command`] for WSL, but with the working directory already
/// in Linux terms (as an agent sends it).
pub async fn build_wsl_command(
    distro: &str,
    program: &str,
    args: &[String],
    linux_cwd: Option<&str>,
    env: &BTreeMap<String, String>,
) -> Command {
    let mut cmd = wsl_command();
    cmd.arg("-d").arg(distro);
    if let Some(cwd) = linux_cwd {
        cmd.arg("--cd").arg(cwd);
    }
    match login_env(distro).await {
        Some(login) => {
            // The shell's variables reach Linux through WSLENV, not the
            // command line, so secrets such as API keys stay out of it.
            if !login.vars.is_empty() {
                let mut names: Vec<String> = std::env::var("WSLENV")
                    .ok()
                    .filter(|v| !v.is_empty())
                    .into_iter()
                    .collect();
                for (k, v) in &login.vars {
                    cmd.env(k, v);
                    names.push(k.clone());
                }
                cmd.env("WSLENV", names.join(":"));
            }
            // `env` sets variables and resolves `program` on the given PATH
            // without a shell, so arguments pass through untouched.
            let path = login.path;
            cmd.arg("--exec").arg("env").arg(format!("PATH={path}"));
            for (k, v) in env {
                cmd.arg(format!("{k}={v}"));
            }
            cmd.arg(program).args(args);
        }
        None => {
            // Fall back to a login shell for PATH.
            cmd.args(["--exec", "env"]);
            for (k, v) in env {
                cmd.arg(format!("{k}={v}"));
            }
            cmd.args(["sh", "-lc", r#"exec "$@""#, "_", program])
                .args(args);
        }
    }
    cmd
}

/// A command for a terminal window: the same program and arguments,
/// written for a POSIX shell and for PowerShell.
#[derive(Debug, Clone)]
pub struct ConsoleScript {
    /// Run with `sh -c` (Linux, macOS, WSL).
    pub unix: String,
    /// Run with `powershell -Command` (native Windows).
    pub windows: String,
}

impl ConsoleScript {
    /// Runs `program args...` (quoted for each shell).
    pub fn program(program: &str, args: &[&str]) -> Self {
        let unix = std::iter::once(program)
            .chain(args.iter().copied())
            .map(sh_quote)
            .collect::<Vec<_>>()
            .join(" ");
        let windows = format!(
            "& {}",
            std::iter::once(program)
                .chain(args.iter().copied())
                .map(ps_quote)
                .collect::<Vec<_>>()
                .join(" ")
        );
        Self { unix, windows }
    }
}

/// Quotes `s` as one POSIX shell word.
pub fn sh_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:@+%,".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Quotes `s` as one PowerShell string literal.
pub fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Opens a visible terminal window running `script` in `target`, so the
/// user can see what runs and answer prompts (sign-in codes etc.). The
/// window waits for Enter before closing when `keep_open` is set; the
/// returned command finishes when the window closes.
///
/// Off Windows there is no portable way to open a terminal, so the script
/// runs without one (stdin closed) and its output is returned instead.
pub async fn build_console_command(
    target: &ExecTarget,
    cwd: &Path,
    script: &ConsoleScript,
    keep_open: bool,
) -> Command {
    let pause_unix = if keep_open {
        r#"; status=$?; echo; printf '[exit %s] Press Enter to close... ' "$status"; read -r _"#
    } else {
        ""
    };
    match target {
        ExecTarget::Wsl { distro } => {
            let linux_cwd = target.to_target_path(cwd);
            let mut cmd = Command::new("wsl.exe");
            cmd.arg("-d").arg(distro).arg("--cd").arg(&linux_cwd);
            let full = format!("{}{pause_unix}", script.unix);
            match login_env(distro).await.map(|l| l.path) {
                Some(path) => {
                    cmd.args(["--exec", "env"])
                        .arg(format!("PATH={path}"))
                        .args(["bash", "-c", &full]);
                }
                None => {
                    cmd.args(["--exec", "bash", "-lc", &full]);
                }
            }
            new_console(&mut cmd);
            cmd
        }
        ExecTarget::Host if cfg!(windows) => {
            let pause = if keep_open {
                "; Write-Host ''; Read-Host \"[exit $LASTEXITCODE] Press Enter to close\""
            } else {
                ""
            };
            let mut cmd = Command::new("powershell.exe");
            cmd.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command"])
                .arg(format!("{}{pause}", script.windows));
            // PowerShell can't start in a UNC directory; the scripts here
            // don't depend on the working directory.
            if !cwd.to_string_lossy().starts_with(r"\\") {
                cmd.current_dir(cwd);
            }
            new_console(&mut cmd);
            cmd
        }
        ExecTarget::Host => {
            let mut cmd = Command::new("sh");
            cmd.args(["-lc", &script.unix]).current_dir(cwd);
            cmd
        }
    }
}

fn new_console(cmd: &mut Command) {
    #[cfg(windows)]
    cmd.creation_flags(0x0000_0010); // CREATE_NEW_CONSOLE
    #[cfg(not(windows))]
    let _ = cmd;
}

/// Whether this platform can open a visible interactive terminal window for
/// provider setup (install / sign-in).
pub const fn supports_console_window() -> bool {
    cfg!(any(windows, target_os = "macos"))
}

/// Builds the contents of a macOS `.command` shell script that runs `script`
/// inside an interactive login shell, publishes its exit code to `done_path`
/// as soon as the command finishes (before any keep-open prompt), and removes
/// `script_path` on exit.
fn macos_command_script(
    cwd: &Path,
    script: &ConsoleScript,
    env: &BTreeMap<String, String>,
    script_path: &Path,
    done_path: &Path,
    pid_path: &Path,
    keep_open: bool,
) -> String {
    let mut done_tmp = done_path.as_os_str().to_os_string();
    done_tmp.push(".tmp");
    let done_tmp_q = sh_quote(&done_tmp.to_string_lossy());
    let done_q = sh_quote(&done_path.to_string_lossy());
    let pid_q = sh_quote(&pid_path.to_string_lossy());
    let script_q = sh_quote(&script_path.to_string_lossy());
    let cwd_q = sh_quote(&cwd.to_string_lossy());
    let cmd_q = sh_quote(&script.unix);

    let mut out = String::new();
    out.push_str("#!/bin/sh\n");
    out.push_str("status=1\n");
    out.push_str("done_written=0\n");
    out.push_str(&format!(
        "printf '%s\\n' \"$$\" > {pid_q} 2>/dev/null || true\n"
    ));
    out.push_str("write_done() {\n");
    out.push_str("  if [ \"$done_written\" -eq 0 ]; then\n");
    out.push_str("    done_written=1\n");
    out.push_str(&format!(
        "    printf '%s\\n' \"$status\" > {done_tmp_q} 2>/dev/null && mv -f {done_tmp_q} {done_q} 2>/dev/null || true\n"
    ));
    out.push_str("  fi\n");
    out.push_str("}\n");
    out.push_str("cleanup() {\n");
    out.push_str("  write_done\n");
    out.push_str(&format!("  rm -f {script_q}\n"));
    out.push_str("}\n");
    out.push_str("trap cleanup EXIT\n");
    out.push_str("trap 'status=130; exit 130' INT\n");
    out.push_str("trap 'status=129; exit 129' HUP\n");
    out.push_str("trap 'status=143; exit 143' TERM\n");
    out.push_str(&format!(
        "cd {cwd_q}\nstatus=$?\nif [ \"$status\" -ne 0 ]; then\n  write_done\n"
    ));
    if keep_open {
        out.push_str("  echo\n");
        out.push_str("  printf '[exit %s] Press Enter to close... ' \"$status\"\n");
        out.push_str("  read -r _\n");
    }
    out.push_str("  exit \"$status\"\n");
    out.push_str("fi\n");
    out.push_str("status=1\n");
    for (k, v) in env {
        if !k.is_empty()
            && !k.as_bytes()[0].is_ascii_digit()
            && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            out.push_str(&format!("export {k}={}\n", sh_quote(v)));
        }
    }
    out.push_str("case \"${SHELL:-}\" in\n");
    out.push_str("  */zsh|*/bash) _same_shell=\"$SHELL\" ;;\n");
    out.push_str("  *)\n");
    out.push_str("    if [ -x /bin/zsh ]; then\n");
    out.push_str("      _same_shell=\"/bin/zsh\"\n");
    out.push_str("    else\n");
    out.push_str("      _same_shell=\"/bin/sh\"\n");
    out.push_str("    fi\n");
    out.push_str("    ;;\n");
    out.push_str("esac\n");
    out.push_str(&format!("\"$_same_shell\" -lic {cmd_q}\n"));
    out.push_str("status=$?\n");
    out.push_str("write_done\n");
    if keep_open {
        out.push_str("echo\n");
        out.push_str("printf '[exit %s] Press Enter to close... ' \"$status\"\n");
        out.push_str("read -r _\n");
    }
    out
}

/// Creates a fresh private temporary directory (`0700` on Unix) so setup
/// scripts and status files cannot collide with or follow symlinks in a shared
/// `TMPDIR`.
fn create_private_temp_dir() -> std::io::Result<PathBuf> {
    static NEXT_ID: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    let base = std::env::temp_dir();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);

    for _ in 0..64 {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let dir = base.join(format!("same-setup-{pid}-{nanos:x}-{id}"));
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "Failed to allocate private temp directory",
    ))
}

/// Opens macOS Terminal.app with an executable `.command` script so the user
/// can interact with login/install prompts, and waits until the command exits
/// (or the window is closed).
pub async fn run_macos_console(
    cwd: &Path,
    script: &ConsoleScript,
    env: &BTreeMap<String, String>,
    keep_open: bool,
) -> std::io::Result<i32> {
    let dir = create_private_temp_dir()?;
    let script_path = dir.join("setup.command");
    let done_path = dir.join("setup.done");
    let pid_path = dir.join("setup.pid");

    let contents = macos_command_script(
        cwd,
        script,
        env,
        &script_path,
        &done_path,
        &pid_path,
        keep_open,
    );
    if let Err(e) = write_executable_script(&script_path, &contents) {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(e);
    }

    let open_out = Command::new("open")
        .args(["-a", "Terminal"])
        .arg(&script_path)
        .stdin(Stdio::null())
        .output()
        .await;
    match open_out {
        Ok(out) if out.status.success() => {}
        Ok(out) => {
            let _ = std::fs::remove_dir_all(&dir);
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return Err(std::io::Error::other(if stderr.is_empty() {
                format!("open -a Terminal exited with {}", out.status)
            } else {
                stderr
            }));
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(e);
        }
    }

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(600);
    loop {
        if let Ok(text) = std::fs::read_to_string(&done_path) {
            let _ = std::fs::remove_dir_all(&dir);
            let code = text.trim().parse::<i32>().unwrap_or(-1);
            return Ok(code);
        }
        if tokio::time::Instant::now() >= deadline {
            if let Ok(pid_text) = std::fs::read_to_string(&pid_path) {
                if let Ok(pid) = pid_text.trim().parse::<u32>() {
                    // Signal both direct children (which may have their own
                    // job-control process group) and the wrapper's process group.
                    let pid_str = pid.to_string();
                    let _ = Command::new("pkill")
                        .args(["-TERM", "-P", &pid_str])
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status()
                        .await;
                    let _ = Command::new("kill")
                        .args(["-TERM", "--", &format!("-{pid}")])
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status()
                        .await;
                }
            }
            let _ = std::fs::remove_dir_all(&dir);
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "Timed out waiting for Terminal",
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

fn write_executable_script(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o700);
    }
    let mut file = opts.open(path)?;
    file.write_all(contents.as_bytes())?;
    Ok(())
}

/// Appends directories from `extra` to `inherited` without duplicating entries
/// or overriding the precedence of `inherited`.
fn merge_unix_paths(inherited: &str, extra: &str) -> String {
    let mut parts: Vec<&str> = inherited.split(':').filter(|p| !p.is_empty()).collect();
    for dir in extra.split(':').filter(|p| !p.is_empty()) {
        if !parts.contains(&dir) {
            parts.push(dir);
        }
    }
    parts.join(":")
}

fn host_path_cache() -> &'static Mutex<Option<Option<String>>> {
    static CACHE: OnceLock<Mutex<Option<Option<String>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

fn host_path_probe_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// Resolves `PATH` from the user's login + interactive shell on a Unix host
/// (so tools added in `~/.zshrc`, `~/.bashrc`, or fish config are found even
/// when the GUI app did not inherit that `PATH`). Single-flight and cached
/// until [`clear_path_cache`].
pub async fn host_login_path() -> Option<String> {
    if !cfg!(unix) {
        return None;
    }
    if let Some(cached) = host_path_cache().lock().unwrap().as_ref() {
        return cached.clone();
    }
    let _guard = host_path_probe_lock().lock().await;
    if let Some(cached) = host_path_cache().lock().unwrap().as_ref() {
        return cached.clone();
    }
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|s| !s.is_empty() && Path::new(s).is_absolute())
        .unwrap_or_else(|| "/bin/sh".to_string());
    // `printenv PATH` prints the colon-separated PATH in POSIX shells and fish alike.
    let probe = format!("printf '\\n{ENV_MARKER}\\n'; printenv PATH");
    let fut = Command::new(&shell)
        .args(["-lic", &probe])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output();
    let path = match tokio::time::timeout(std::time::Duration::from_secs(5), fut).await {
        Ok(Ok(out)) => {
            let marker = format!("\n{ENV_MARKER}\n");
            let text = String::from_utf8_lossy(&out.stdout);
            text.find(&marker).and_then(|pos| {
                let rest = &text[pos + marker.len()..];
                let line = rest.lines().next()?.trim();
                (!line.is_empty()).then(|| line.to_string())
            })
        }
        _ => None,
    };
    *host_path_cache().lock().unwrap() = Some(path.clone());
    path
}

/// Forgets cached login-shell environments, e.g. after installing a CLI whose
/// installer added a directory to the shell profile.
pub fn clear_path_cache() {
    path_cache().lock().unwrap().clear();
    *host_path_cache().lock().unwrap() = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_env_parsing() {
        let vars = parse_env0(b"\nHOME=/home/a\0OPENAI_API_KEY=sk=1\0MULTI=a\nb\0");
        assert_eq!(
            vars,
            [
                ("HOME".to_string(), "/home/a".to_string()),
                ("OPENAI_API_KEY".to_string(), "sk=1".to_string()),
                ("MULTI".to_string(), "a\nb".to_string()),
            ]
        );
        assert!(forwardable("GOOGLE_API_KEY"));
        assert!(!forwardable("PATH"));
        assert!(!forwardable("WSL_DISTRO_NAME"));
        assert!(!forwardable("BASH_FUNC_x%%"));
    }

    #[test]
    fn quotes_console_scripts() {
        let s = ConsoleScript::program("agy", &["acp", "--login"]);
        assert_eq!(s.unix, "agy acp --login");
        assert_eq!(s.windows, "& 'agy' 'acp' '--login'");
        let s = ConsoleScript::program(r"C:\Users\a b\agy.exe", &["it's"]);
        assert_eq!(s.windows, r"& 'C:\Users\a b\agy.exe' 'it''s'");
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
        assert_eq!(sh_quote(""), "''");
    }

    #[test]
    fn builds_macos_command_script() {
        let script = ConsoleScript::program("agy", &["acp", "--login"]);
        let mut env = BTreeMap::new();
        env.insert("VALID_KEY".to_string(), "a'b c".to_string());
        env.insert("BAD-KEY".to_string(), "ignored".to_string());
        let text = macos_command_script(
            Path::new("/tmp/my proj"),
            &script,
            &env,
            Path::new("/tmp/run.command"),
            Path::new("/tmp/run.done"),
            Path::new("/tmp/run.pid"),
            true,
        );
        assert!(text.starts_with("#!/bin/sh\n"));
        assert!(text.contains("cd '/tmp/my proj'\nstatus=$?\nif [ \"$status\" -ne 0 ]; then\n"));
        assert!(text.contains("export VALID_KEY='a'\\''b c'\n"));
        assert!(!text.contains("BAD-KEY"));
        assert!(text.contains("\"$_same_shell\" -lic 'agy acp --login'\n"));
        assert!(text.contains("Press Enter to close..."));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn macos_command_script_writes_exit_code_and_cleans_up() {
        let dir = create_private_temp_dir().unwrap();
        let script_path = dir.join("test.command");
        let done_path = dir.join("test.done");
        let pid_path = dir.join("test.pid");
        let out_path = dir.join("out.txt");

        let script = ConsoleScript {
            unix: format!(
                "printf '%s' \"$SAME_TEST_ENV\" > {}; exit 42",
                sh_quote(&out_path.to_string_lossy())
            ),
            windows: String::new(),
        };
        let mut env = BTreeMap::new();
        env.insert("SAME_TEST_ENV".to_string(), "hello macos".to_string());

        let contents = macos_command_script(
            &dir,
            &script,
            &env,
            &script_path,
            &done_path,
            &pid_path,
            false,
        );
        write_executable_script(&script_path, &contents).unwrap();

        let status = Command::new(&script_path)
            .env("SHELL", "/bin/sh")
            .stdin(Stdio::null())
            .status()
            .await
            .unwrap();
        assert!(status.success());
        assert_eq!(std::fs::read_to_string(&done_path).unwrap().trim(), "42");
        assert_eq!(std::fs::read_to_string(&out_path).unwrap(), "hello macos");
        assert!(!script_path.exists(), "script should self-delete on exit");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_wsl_unc_paths() {
        assert_eq!(
            wsl_location(r"\\wsl.localhost\Ubuntu-20.04\home\user\git\same"),
            Some(("Ubuntu-20.04".into(), "/home/user/git/same".into()))
        );
        assert_eq!(
            wsl_location(r"\\wsl$\Ubuntu\home\x\"),
            Some(("Ubuntu".into(), "/home/x".into()))
        );
        assert_eq!(
            wsl_location(r"\\?\UNC\wsl.localhost\Debian\srv"),
            Some(("Debian".into(), "/srv".into()))
        );
        assert_eq!(
            wsl_location(r"\\WSL.LOCALHOST\Ubuntu"),
            Some(("Ubuntu".into(), "/".into()))
        );
        assert_eq!(wsl_location(r"C:\work"), None);
        assert_eq!(wsl_location("/home/user"), None);
        assert_eq!(wsl_location(r"\\server\share\x"), None);
    }

    #[test]
    fn maps_paths_between_windows_and_wsl() {
        let t = ExecTarget::Wsl {
            distro: "Ubuntu-20.04".into(),
        };
        assert_eq!(
            t.to_target_path(Path::new(r"\\wsl.localhost\Ubuntu-20.04\home\user\p")),
            "/home/user/p"
        );
        assert_eq!(
            t.to_target_path(Path::new(r"C:\work\repo")),
            "/mnt/c/work/repo"
        );
        assert_eq!(
            t.to_host_path(Path::new("ignored"), "/home/user/p/main.rs"),
            PathBuf::from(r"\\wsl.localhost\Ubuntu-20.04\home\user\p\main.rs")
        );
        assert_eq!(
            t.to_host_path(Path::new("ignored"), "/mnt/d/data/x.txt"),
            PathBuf::from(r"D:\data\x.txt")
        );
        assert_eq!(
            t.to_host_path(Path::new("base"), "src/lib.rs"),
            Path::new("base").join("src/lib.rs")
        );
    }

    #[tokio::test]
    async fn off_windows_everything_runs_on_the_host() {
        if cfg!(windows) {
            return;
        }
        let cwd = Path::new(r"\\wsl.localhost\Ubuntu\home");
        assert_eq!(
            resolve_target(ExecPreference::Wsl, cwd).await,
            ExecTarget::Host
        );
    }

    /// Real WSL round trip. Run on Windows with
    /// `SAME_WSL_TEST_DIR=\\wsl.localhost\<distro>\<dir>` and `--ignored`.
    #[cfg(windows)]
    #[tokio::test]
    #[ignore]
    async fn runs_commands_inside_wsl() {
        let dir = std::env::var("SAME_WSL_TEST_DIR").expect("SAME_WSL_TEST_DIR");
        let cwd = Path::new(&dir);
        let target = resolve_target(ExecPreference::Auto, cwd).await;
        let (distro, linux) = wsl_location(&dir).unwrap();
        assert_eq!(
            target,
            ExecTarget::Wsl {
                distro: distro.clone()
            }
        );

        // Working directory and login-shell PATH.
        let mut env = BTreeMap::new();
        env.insert("SAME_TEST_VAR".to_string(), "hello world".to_string());
        let script = "pwd; printf '%s\\n' \"$SAME_TEST_VAR\"; command -v claude || echo no-claude";
        let out = build_command(
            &target,
            "sh",
            &["-c".to_string(), script.to_string()],
            cwd,
            &env,
        )
        .await
        .output()
        .await
        .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        eprintln!(
            "stdout: {stdout}\nstderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let mut lines = stdout.lines();
        assert_eq!(lines.next(), Some(linux.as_str()));
        assert_eq!(lines.next(), Some("hello world"));
        assert!(
            lines.next().unwrap().starts_with('/'),
            "claude not on login PATH"
        );

        // A Linux path the agent sends maps back to a readable host path.
        let host = target.to_host_path(cwd, &format!("{linux}/Cargo.toml"));
        assert!(std::fs::read_to_string(&host)
            .unwrap()
            .contains("[workspace]"));

        // Dropping the child must stop the Linux process too.
        let mut child = build_command(
            &target,
            "sleep",
            &["31337".to_string()],
            cwd,
            &BTreeMap::new(),
        )
        .await
        .kill_on_drop(true)
        .spawn()
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        child.kill().await.unwrap();
        drop(child);
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let check = build_command(
            &target,
            "sh",
            &[
                "-c".to_string(),
                "pgrep -f 'sleep 3133[7]' || echo gone".to_string(),
            ],
            cwd,
            &BTreeMap::new(),
        )
        .await
        .output()
        .await
        .unwrap();
        assert_eq!(String::from_utf8_lossy(&check.stdout).trim(), "gone");
    }

    /// Opens real console windows (they close by themselves). Run on Windows
    /// with `SAME_WSL_TEST_DIR` and `--ignored`.
    #[cfg(windows)]
    #[tokio::test]
    #[ignore]
    async fn console_windows_run_scripts() {
        let dir = std::env::var("SAME_WSL_TEST_DIR").expect("SAME_WSL_TEST_DIR");
        let cwd = Path::new(&dir);
        let wsl = resolve_target(ExecPreference::Auto, cwd).await;
        let script = ConsoleScript {
            unix: "pwd > /tmp/same_console_test && command -v claude >> /tmp/same_console_test"
                .to_string(),
            windows: String::new(),
        };
        let status = build_console_command(&wsl, cwd, &script, false)
            .await
            .status()
            .await
            .unwrap();
        assert!(status.success());
        let (distro, linux) = wsl_location(&dir).unwrap();
        let written =
            std::fs::read_to_string(format!(r"\\wsl.localhost\{distro}\tmp\same_console_test"))
                .unwrap();
        assert!(written.starts_with(&linux), "{written}");
        assert!(written.contains("/claude"), "{written}");

        let out = std::env::temp_dir().join("same_console_test.txt");
        let _ = std::fs::remove_file(&out);
        let script = ConsoleScript {
            unix: String::new(),
            windows: format!(
                "Set-Content -Path {} -Value 'ok from powershell'",
                ps_quote(&out.to_string_lossy())
            ),
        };
        let status = build_console_command(&ExecTarget::Host, cwd, &script, false)
            .await
            .status()
            .await
            .unwrap();
        assert!(status.success());
        assert!(std::fs::read_to_string(&out)
            .unwrap()
            .contains("ok from powershell"));
    }
}
