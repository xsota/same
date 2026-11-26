use anyhow::{anyhow, bail, Context, Result};
use same_acp::exec::{build_command, ExecTarget};
use same_acp::{AcpClient, AcpClientConfig, AcpEvent, SessionUpdate};
use same_core::skills::validator::{parse_skill_md, slugify_skill_name, validate_skill_name};
use same_core::{ProviderConfig, ProviderKind, SkillScope};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

const GENERATE_TIMEOUT: Duration = Duration::from_secs(120);

/// AI-generated draft of a new Agent Skill ready to populate the manual editor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedSkillDraft {
    pub name: String,
    pub description: String,
    pub body: String,
}

/// Asks `provider` to draft an Agent Skills `SKILL.md` based on `user_request`
/// (and optional existing manual draft fields for iterative refinement).
///
/// Runs non-interactively in read-only mode without executing tools or writing files.
pub async fn generate_skill_draft(
    provider: &ProviderConfig,
    target: &ExecTarget,
    cwd: &Path,
    user_request: &str,
    scope: SkillScope,
    existing_draft: Option<(&str, &str, &str)>,
) -> Result<GeneratedSkillDraft> {
    let prompt = build_generation_prompt(user_request, scope, existing_draft);
    let fut = async {
        match provider.kind {
            ProviderKind::ClaudeCli => run_claude_oneshot(provider, target, cwd, &prompt).await,
            ProviderKind::CodexCli => run_codex_oneshot(provider, target, cwd, &prompt).await,
            ProviderKind::Acp => run_acp_oneshot(provider, target, cwd, &prompt).await,
        }
    };
    let raw = tokio::time::timeout(GENERATE_TIMEOUT, fut)
        .await
        .map_err(|_| anyhow!("AI generation timed out after {}s", GENERATE_TIMEOUT.as_secs()))??;
    extract_skill_draft_from_ai_output(&raw, user_request)
}

fn build_generation_prompt(
    user_request: &str,
    scope: SkillScope,
    existing_draft: Option<(&str, &str, &str)>,
) -> String {
    let scope_note = match scope {
        SkillScope::Global => "Global skill (reusable across multiple projects)",
        SkillScope::Project => "Project-specific skill (tailored to the current repository)",
    };

    let mut prompt = format!(
        "You are an expert author of Agent Skills (https://agentskills.io/specification).\n\
         Draft a high-quality SKILL.md based on the user's request below.\n\
         \n\
         Strict output rules:\n\
         - Do NOT run shell commands, edit files, or call any tools.\n\
         - Output ONLY the raw SKILL.md document (starting with `---` YAML frontmatter and ending with the Markdown body).\n\
         - Frontmatter MUST contain:\n\
           - `name`: lowercase ASCII letters, digits, and single hyphens only (max 64 chars, e.g. `pr-security-review`).\n\
           - `description`: concise, clear explanation of WHAT the skill does and WHEN the agent should activate it.\n\
         - Write the Markdown instructions in the same language as the user's request.\n\
         - Keep instructions actionable, structured (steps, checklists, constraints, verification), and concise.\n\
         - Scope: {scope_note}\n"
    );

    if let Some((name, desc, body)) = existing_draft {
        if !name.trim().is_empty() || !desc.trim().is_empty() || !body.trim().is_empty() {
            prompt.push_str("\nCurrent manual draft to refine/improve:\n");
            if !name.trim().is_empty() {
                prompt.push_str(&format!("- Current name: {}\n", name.trim()));
            }
            if !desc.trim().is_empty() {
                prompt.push_str(&format!("- Current description: {}\n", desc.trim()));
            }
            if !body.trim().is_empty() {
                prompt.push_str(&format!("- Current instructions:\n{body}\n"));
            }
        }
    }

    prompt.push_str("\nUser request:\n");
    prompt.push_str(user_request.trim());
    prompt.push('\n');
    prompt
}

async fn run_claude_oneshot(
    provider: &ProviderConfig,
    target: &ExecTarget,
    cwd: &Path,
    prompt: &str,
) -> Result<String> {
    let mut args = provider.args.clone();
    args.extend([
        "-p".to_string(),
        "--output-format".to_string(),
        "text".to_string(),
        "--permission-mode".to_string(),
        "plan".to_string(),
    ]);
    let mut cmd = build_command(target, &provider.command, &args, cwd, &provider.env).await;
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd
        .spawn()
        .with_context(|| format!("Failed to start '{}'", provider.command))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(prompt.as_bytes()).await?;
        let _ = stdin.shutdown().await;
    }
    let out = child.wait_with_output().await?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() && stdout.is_empty() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        bail!(
            "Claude CLI failed ({}): {}",
            out.status.code().unwrap_or(-1),
            stderr
        );
    }
    Ok(stdout)
}

async fn run_codex_oneshot(
    provider: &ProviderConfig,
    target: &ExecTarget,
    cwd: &Path,
    prompt: &str,
) -> Result<String> {
    let mut args = provider.args.clone();
    args.extend([
        "exec".to_string(),
        "--sandbox".to_string(),
        "read-only".to_string(),
        "--skip-git-repo-check".to_string(),
        "-".to_string(),
    ]);
    let mut cmd = build_command(target, &provider.command, &args, cwd, &provider.env).await;
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd
        .spawn()
        .with_context(|| format!("Failed to start '{}'", provider.command))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(prompt.as_bytes()).await?;
        let _ = stdin.shutdown().await;
    }
    let out = child.wait_with_output().await?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() && stdout.is_empty() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        bail!(
            "Codex CLI failed ({}): {}",
            out.status.code().unwrap_or(-1),
            stderr
        );
    }
    Ok(stdout)
}

async fn run_acp_oneshot(
    provider: &ProviderConfig,
    target: &ExecTarget,
    cwd: &Path,
    prompt: &str,
) -> Result<String> {
    let (client, mut rx) = AcpClient::spawn_read_only(AcpClientConfig {
        command: provider.command.clone(),
        args: provider.args.clone(),
        env: provider.env.clone(),
        cwd: cwd.to_path_buf(),
        auto_approve: false,
        target: target.clone(),
    })
    .await?;

    let session = match client.new_session(cwd).await {
        Ok(s) => s,
        Err(e) => {
            client.shutdown().await;
            return Err(e);
        }
    };

    let collector = tokio::spawn(async move {
        let mut buf = String::new();
        while let Some(ev) = rx.recv().await {
            if let AcpEvent::SessionUpdate {
                update: SessionUpdate::AgentMessageChunk { text },
                ..
            } = ev
            {
                buf.push_str(&text);
            }
        }
        buf
    });

    let prompt_res = client.prompt(&session.session_id, prompt).await;
    client.shutdown().await;
    let text = collector.await.unwrap_or_default();
    if let Err(e) = prompt_res {
        if text.trim().is_empty() {
            return Err(e);
        }
    }
    Ok(text.trim().to_string())
}

/// Extracts `name`, `description`, and Markdown `body` from an AI response that
/// may contain raw `SKILL.md`, a fenced code block, or brief surrounding prose.
pub fn extract_skill_draft_from_ai_output(
    raw: &str,
    fallback_request: &str,
) -> Result<GeneratedSkillDraft> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("AI returned an empty response");
    }

    // 1. Try parsing the raw document directly (or from a leading `---` line)
    //    BEFORE stripping code fences, so fenced examples inside the skill body
    //    are never mistaken for an outer wrapper.
    let direct_candidates = [
        Some(trimmed),
        find_frontmatter_start(trimmed).map(|pos| &trimmed[pos..]),
    ];
    for candidate in direct_candidates.into_iter().flatten() {
        if let Ok(parsed) = parse_skill_md(candidate) {
            return Ok(build_draft_from_parsed(parsed, fallback_request));
        }
    }

    // 2. Otherwise try unwrapping an outer fenced code block (e.g. ```markdown ... ```).
    if let Some(unwrapped) = unwrap_fenced_block(trimmed) {
        let fenced_candidates = [
            Some(unwrapped),
            find_frontmatter_start(unwrapped).map(|pos| &unwrapped[pos..]),
        ];
        for candidate in fenced_candidates.into_iter().flatten() {
            if let Ok(parsed) = parse_skill_md(candidate) {
                return Ok(build_draft_from_parsed(parsed, fallback_request));
            }
        }
        return Ok(GeneratedSkillDraft {
            name: slugify_skill_name(fallback_request),
            description: fallback_request.trim().chars().take(160).collect(),
            body: unwrapped.to_string(),
        });
    }

    // 3. Fallback: AI returned plain Markdown instructions without frontmatter.
    Ok(GeneratedSkillDraft {
        name: slugify_skill_name(fallback_request),
        description: fallback_request.trim().chars().take(160).collect(),
        body: trimmed.to_string(),
    })
}

fn build_draft_from_parsed(
    parsed: same_core::ParsedSkillMd,
    fallback_request: &str,
) -> GeneratedSkillDraft {
    let name = if validate_skill_name(&parsed.name).is_ok() {
        parsed.name
    } else {
        slugify_skill_name(&parsed.name)
    };
    let description = if !parsed.description.trim().is_empty() {
        parsed.description.trim().to_string()
    } else {
        fallback_request.trim().chars().take(160).collect()
    };
    GeneratedSkillDraft {
        name,
        description,
        body: parsed.body,
    }
}

fn unwrap_fenced_block(text: &str) -> Option<&str> {
    let fence_start = text.find("```")?;
    let after_fence = &text[fence_start + 3..];
    let nl = after_fence.find('\n')?;
    let body = &after_fence[nl + 1..];
    let fence_end = body.rfind("\n```").or_else(|| body.rfind("```"))?;
    Some(body[..fence_end].trim())
}

fn find_frontmatter_start(text: &str) -> Option<usize> {
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        if line.trim() == "---" {
            return Some(offset);
        }
        offset += line.len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_raw_and_fenced_skill_md() {
        let raw = "---\nname: pr-security-review\ndescription: Checks PRs for security bugs\n---\n\n# PR Security Review\n\n1. Inspect auth.\n";
        let d = extract_skill_draft_from_ai_output(raw, "security").unwrap();
        assert_eq!(d.name, "pr-security-review");
        assert_eq!(d.description, "Checks PRs for security bugs");
        assert!(d.body.contains("# PR Security Review"));

        let fenced = format!("Here is your skill:\n\n```markdown\n{raw}```\n");
        let d2 = extract_skill_draft_from_ai_output(&fenced, "security").unwrap();
        assert_eq!(d2.name, "pr-security-review");
        assert_eq!(d2.description, "Checks PRs for security bugs");
        assert!(d2.body.contains("1. Inspect auth."));
    }

    #[test]
    fn sanitizes_invalid_name_and_handles_plain_markdown_fallback() {
        let bad_name = "---\nname: My Cool Skill!\ndescription: Does cool things\n---\n\n# Body\n";
        let d = extract_skill_draft_from_ai_output(bad_name, "cool").unwrap();
        assert_eq!(d.name, "my-cool-skill");
        assert_eq!(d.description, "Does cool things");

        let plain = "# Custom Checklist\n\n- Step 1\n- Step 2\n";
        let d2 = extract_skill_draft_from_ai_output(plain, "release checklist").unwrap();
        assert_eq!(d2.name, "release-checklist");
        assert!(d2.body.contains("Step 1"));

        // Raw SKILL.md that contains a fenced code example inside its body must NOT
        // be reduced to the inner code block.
        let with_inner_fence = "---\nname: rust-lint\ndescription: Runs clippy\n---\n\n# Rust Lint\n\nRun:\n```bash\ncargo clippy\n```\n";
        let d3 = extract_skill_draft_from_ai_output(with_inner_fence, "lint").unwrap();
        assert_eq!(d3.name, "rust-lint");
        assert_eq!(d3.description, "Runs clippy");
        assert!(d3.body.contains("# Rust Lint"));
        assert!(d3.body.contains("cargo clippy"));
    }
}
