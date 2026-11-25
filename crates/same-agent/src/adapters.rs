use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStdin};
use tokio::sync::Mutex;

/// Collects a command line so it can be launched in any [`ExecTarget`].
pub(crate) struct ArgList(pub(crate) Vec<String>);

impl ArgList {
    pub(crate) fn new(base: &[String]) -> Self {
        Self(base.to_vec())
    }

    pub(crate) fn arg(&mut self, a: impl Into<String>) -> &mut Self {
        self.0.push(a.into());
        self
    }
}

/// A child's stdin shared by the tasks that answer its requests; `None` once
/// closed.
pub(crate) type SharedStdin = Arc<Mutex<Option<ChildStdin>>>;

pub(crate) fn shared_stdin(child: &mut Child) -> Result<SharedStdin> {
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("Failed to capture agent stdin"))?;
    Ok(Arc::new(Mutex::new(Some(stdin))))
}

/// Writes `val` as one JSON line.
pub(crate) async fn write_json_line(stdin: &SharedStdin, val: &Value) -> Result<()> {
    let mut line = serde_json::to_string(val)?;
    line.push('\n');
    let mut guard = stdin.lock().await;
    let Some(pipe) = guard.as_mut() else {
        bail!("Agent stdin is already closed");
    };
    pipe.write_all(line.as_bytes())
        .await
        .context("Failed to write to agent stdin")?;
    pipe.flush().await.context("Failed to flush agent stdin")?;
    Ok(())
}

/// Closes stdin so the agent sees the end of input.
pub(crate) async fn close_stdin(stdin: &SharedStdin) {
    if let Some(mut pipe) = stdin.lock().await.take() {
        let _ = pipe.shutdown().await;
    }
}

/// Whether an attachment is an image the agent can look at directly.
pub fn is_image_path(path: &str) -> bool {
    let ext = path
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    matches!(
        ext.as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp"
    )
}

/// Appends attachments as Claude Code `@path` mentions, which it expands
/// into the files' contents (images included). `paths` are in the agent's
/// terms (see [`ExecTarget::to_target_path`]).
pub fn claude_prompt_with_attachments(prompt: &str, paths: &[String]) -> String {
    if paths.is_empty() {
        return prompt.to_string();
    }
    let mentions: Vec<String> = paths
        .iter()
        .map(|p| {
            if p.contains(char::is_whitespace) {
                format!("@\"{p}\"")
            } else {
                format!("@{p}")
            }
        })
        .collect();
    format!("{prompt}\n\n{}", mentions.join("\n"))
}

/// Appends the attachment paths for Codex, which reads files with its own
/// tools; images are also passed with `--image`.
pub fn codex_prompt_with_attachments(prompt: &str, paths: &[String]) -> String {
    if paths.is_empty() {
        return prompt.to_string();
    }
    let list: Vec<String> = paths.iter().map(|p| format!("- {p}")).collect();
    format!("{prompt}\n\nAttached files:\n{}", list.join("\n"))
}

/// Lines of stderr kept to explain a failed run.
pub(crate) const STDERR_TAIL_LINES: usize = 20;

/// Builds a one-line tool card title such as `Read src/main.rs` or
/// `Bash: cargo test` from a Claude `tool_use` block.
pub(crate) fn summarize_tool_call(name: &str, input: Option<&Value>) -> String {
    let field = |k: &str| {
        input
            .and_then(|i| i.get(k))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    let detail = field("file_path")
        .or_else(|| field("notebook_path"))
        .or_else(|| field("path"))
        .or_else(|| field("command"))
        .or_else(|| field("pattern"))
        .or_else(|| field("url"))
        .or_else(|| field("query"))
        .or_else(|| field("description"));
    match detail {
        Some(d) => {
            let first = d.lines().next().unwrap_or(d);
            let mut short: String = first.chars().take(80).collect();
            if first.chars().count() > 80 || d.lines().count() > 1 {
                short.push('…');
            }
            format!("{name} {short}")
        }
        None => name.to_string(),
    }
}

#[cfg(test)]
mod summarize_tests {
    use super::summarize_tool_call;
    use serde_json::json;

    #[test]
    fn summarizes_common_tool_inputs() {
        assert_eq!(
            summarize_tool_call("Read", Some(&json!({"file_path": "src/main.rs"}))),
            "Read src/main.rs"
        );
        assert_eq!(
            summarize_tool_call("Bash", Some(&json!({"command": "cargo test\necho hi"}))),
            "Bash cargo test…"
        );
        assert_eq!(
            summarize_tool_call("TodoWrite", Some(&json!({"todos": []}))),
            "TodoWrite"
        );
        assert_eq!(summarize_tool_call("X", None), "X");
    }
}
