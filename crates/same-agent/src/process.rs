//! Long-lived agent processes speaking JSON lines on stdio (Claude Code's
//! `stream-json`, `codex app-server`), kept per SAME session between turns
//! and released when idle.

use crate::adapters::{close_stdin, shared_stdin, SharedStdin, STDERR_TAIL_LINES};
use anyhow::{anyhow, Context, Result};
use same_acp::exec::{build_command, ExecTarget};
use same_core::{EventBus, ProviderConfig, SameEvent};
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Child;
use tokio::sync::{mpsc, Mutex};

/// How long a process may sit unused before it is shut down. The next turn
/// resumes the conversation in a new one.
pub(crate) const IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// A child process whose stdout is read line by line in the background.
pub(crate) struct LineProcess {
    child: Child,
    pub stdin: SharedStdin,
    /// Stdout lines; `None` once the process closed stdout (exited).
    pub lines: mpsc::UnboundedReceiver<String>,
    stderr_tail: Arc<std::sync::Mutex<VecDeque<String>>>,
}

impl LineProcess {
    pub async fn spawn(
        provider: &ProviderConfig,
        args: &[String],
        cwd: &Path,
        target: &ExecTarget,
        events: &EventBus,
        session_id: &str,
    ) -> Result<Self> {
        let mut cmd = build_command(target, &provider.command, args, cwd, &provider.env).await;
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .with_context(|| format!("Failed to spawn '{}'", provider.command))?;
        let stdin = shared_stdin(&mut child)?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("Failed to capture agent stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("Failed to capture agent stderr"))?;

        let (tx, lines) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });

        let stderr_tail = Arc::new(std::sync::Mutex::new(VecDeque::new()));
        {
            let events = events.clone();
            let sid = session_id.to_string();
            let pid = provider.id.clone();
            let tail = stderr_tail.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    {
                        let mut tail = tail.lock().unwrap();
                        if tail.len() == STDERR_TAIL_LINES {
                            tail.pop_front();
                        }
                        tail.push_back(line.clone());
                    }
                    events.publish(SameEvent::AgentLog {
                        session_id: Some(sid.clone()),
                        provider_id: pid.clone(),
                        line,
                    });
                }
            });
        }

        Ok(Self {
            child,
            stdin,
            lines,
            stderr_tail,
        })
    }

    /// Discards output left over from an earlier turn.
    pub fn drain(&mut self) {
        while self.lines.try_recv().is_ok() {}
    }

    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    pub fn stderr_tail(&self) -> Vec<String> {
        self.stderr_tail.lock().unwrap().iter().cloned().collect()
    }

    /// Waits briefly for the process to exit after stdout closed, so its
    /// exit code and last stderr lines are available.
    pub async fn wait_exit(&mut self) -> Option<i32> {
        let status = tokio::time::timeout(Duration::from_secs(3), self.child.wait()).await;
        // Let stderr catch up.
        tokio::time::sleep(Duration::from_millis(100)).await;
        status.ok().and_then(Result::ok).and_then(|s| s.code())
    }

    /// Closes stdin so the agent can exit on its own (a kill through
    /// `wsl.exe` may not reach the Linux process), then kills it if needed.
    pub async fn shutdown(mut self) {
        close_stdin(&self.stdin).await;
        if tokio::time::timeout(Duration::from_secs(3), self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.kill().await;
        }
    }
}

/// A process kept for one SAME session.
pub(crate) trait Pooled: Send + 'static {
    fn process(&mut self) -> &mut LineProcess;
    fn into_process(self) -> LineProcess;
    /// What the process was started with; a different one needs a new
    /// process.
    fn fingerprint(&self) -> &str;
}

struct Slot<T> {
    process: Option<T>,
    last_used: Instant,
}

/// Per-session processes. A turn holds its session's slot for its whole
/// run, so the idle reaper (which only tries the lock) never stops a process
/// mid-turn.
pub(crate) struct Pool<T> {
    slots: std::sync::Mutex<HashMap<String, Arc<Mutex<Slot<T>>>>>,
}

pub(crate) struct Lease<T: 'static> {
    guard: tokio::sync::OwnedMutexGuard<Slot<T>>,
}

impl<T: Pooled> Lease<T> {
    /// The live process for this fingerprint, if there is one; a dead or
    /// differently started process is shut down first.
    pub async fn reusable(&mut self, fingerprint: &str) -> Option<&mut T> {
        let keep = match self.guard.process.as_mut() {
            Some(p) => p.fingerprint() == fingerprint && p.process().is_alive(),
            None => false,
        };
        if !keep {
            if let Some(old) = self.guard.process.take() {
                shutdown_pooled(old).await;
            }
            return None;
        }
        self.guard.process.as_mut()
    }

    pub fn put(&mut self, process: T) -> &mut T {
        self.guard.process.insert(process)
    }

    pub fn get(&mut self) -> Option<&mut T> {
        self.guard.process.as_mut()
    }

    /// Drops the process after a failure that leaves it unusable.
    pub async fn discard(&mut self) {
        if let Some(old) = self.guard.process.take() {
            shutdown_pooled(old).await;
        }
    }
}

impl<T> Drop for Lease<T> {
    fn drop(&mut self) {
        self.guard.last_used = Instant::now();
    }
}

async fn shutdown_pooled<T: Pooled>(p: T) {
    p.into_process().shutdown().await;
}

impl<T: Pooled> Default for Pool<T> {
    fn default() -> Self {
        Self {
            slots: std::sync::Mutex::new(HashMap::new()),
        }
    }
}

impl<T: Pooled> Pool<T> {
    /// Waits for the session's slot (another turn of it may still be ending).
    pub async fn lease(&self, session_id: &str) -> Lease<T> {
        let slot = self
            .slots
            .lock()
            .unwrap()
            .entry(session_id.to_string())
            .or_insert_with(|| {
                Arc::new(Mutex::new(Slot {
                    process: None,
                    last_used: Instant::now(),
                }))
            })
            .clone();
        Lease {
            guard: slot.lock_owned().await,
        }
    }

    /// Shuts down the session's process (deleted session, switched agent).
    pub async fn release(&self, session_id: &str) {
        let slot = self.slots.lock().unwrap().remove(session_id);
        if let Some(slot) = slot {
            let mut guard = slot.lock().await;
            if let Some(p) = guard.process.take() {
                shutdown_pooled(p).await;
            }
        }
    }

    /// Shuts down processes idle longer than `timeout`; busy ones are skipped.
    pub async fn reap_idle(&self, timeout: Duration) {
        let slots: Vec<Arc<Mutex<Slot<T>>>> =
            self.slots.lock().unwrap().values().cloned().collect();
        for slot in slots {
            let Ok(mut guard) = slot.try_lock() else {
                continue;
            };
            if guard.process.is_some() && guard.last_used.elapsed() >= timeout {
                if let Some(p) = guard.process.take() {
                    shutdown_pooled(p).await;
                }
            }
        }
    }

    pub async fn shutdown_all(&self) {
        let slots: Vec<Arc<Mutex<Slot<T>>>> =
            self.slots.lock().unwrap().drain().map(|(_, s)| s).collect();
        for slot in slots {
            let mut guard = slot.lock().await;
            if let Some(p) = guard.process.take() {
                shutdown_pooled(p).await;
            }
        }
    }
}
