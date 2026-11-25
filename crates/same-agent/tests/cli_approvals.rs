//! End-to-end approval flow against the real `claude` and `codex` CLIs.
//! They cost tokens and need logged-in CLIs, so they are ignored by default:
//! `cargo test -p same-agent --test cli_approvals -- --ignored --test-threads=1`

use same_agent::{AgentManager, ExecPreference, PromptRequest};
use same_core::config::default_providers;
use same_core::{EventBus, PermissionRequest, SameEvent, ToolCallStatus};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Clone, Copy)]
enum Answer {
    /// Choose the first option of this kind.
    Pick(&'static str),
    /// Stop the run while the prompt is open (as the Stop button does).
    Stop,
}

#[derive(Debug, PartialEq)]
enum End {
    Completed,
    Cancelled,
    Failed(String),
}

struct Outcome {
    approvals: Vec<PermissionRequest>,
    end: End,
    external_id: Option<String>,
    statuses: Vec<ToolCallStatus>,
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("same-approvals-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[derive(Default, Clone, Copy)]
struct Opts {
    model: Option<&'static str>,
    effort: Option<&'static str>,
    auto_approve: bool,
}

fn model(m: &'static str) -> Opts {
    Opts {
        model: Some(m),
        ..Opts::default()
    }
}

async fn run(
    provider_id: &str,
    opts: Opts,
    cwd: &Path,
    prompt: &str,
    external_id: Option<String>,
    answer: Answer,
) -> Outcome {
    let events = EventBus::new(1024);
    let mut rx = events.subscribe();
    let mgr = AgentManager::new(events, tokio::runtime::Handle::current());
    mgr.set_auto_approve(opts.auto_approve);
    let provider = default_providers()
        .into_iter()
        .find(|p| p.id == provider_id)
        .unwrap();
    let sid = format!("e2e-{provider_id}");
    mgr.start_prompt(PromptRequest {
        session_id: sid.clone(),
        external_session_id: external_id,
        provider,
        model: opts.model.map(str::to_string),
        effort: opts.effort.map(str::to_string),
        cwd: cwd.to_path_buf(),
        prompt: prompt.to_string(),
        attachments: vec![],
        exec: ExecPreference::Auto,
    });

    let mut out = Outcome {
        approvals: vec![],
        end: End::Failed("timed out".into()),
        external_id: None,
        statuses: vec![],
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(240);
    loop {
        let ev = match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Ok(ev)) => ev,
            _ => break,
        };
        match ev {
            SameEvent::ApprovalRequested { request } if request.session_id == sid => {
                out.approvals.push(request.clone());
                match answer {
                    Answer::Pick(kind) => {
                        let opt = request
                            .options
                            .iter()
                            .find(|o| o.kind == kind)
                            .unwrap_or_else(|| panic!("no {kind} option in {:?}", request.options));
                        mgr.respond_permission(&request.request_id, Some(opt.id.clone()))
                            .await;
                    }
                    // The app's order: answer the prompt, then cancel.
                    Answer::Stop => {
                        mgr.respond_permission(&request.request_id, None).await;
                        mgr.cancel_session(&sid).await;
                    }
                }
            }
            SameEvent::ExternalSessionBound {
                session_id,
                external_session_id,
            } if session_id == sid => out.external_id = Some(external_session_id),
            SameEvent::ToolCallUpdated {
                session_id,
                tool_call,
            } if session_id == sid => out.statuses.push(tool_call.status),
            SameEvent::SessionCompleted { session_id, .. } if session_id == sid => {
                out.end = End::Completed;
                break;
            }
            SameEvent::SessionCancelled { session_id } if session_id == sid => {
                out.end = End::Cancelled;
                break;
            }
            SameEvent::SessionFailed { session_id, error } if session_id == sid => {
                out.end = End::Failed(error);
                break;
            }
            _ => {}
        }
    }
    out
}

const TOUCH: &str = "Run exactly this shell command with your shell tool: `touch made.txt`. \
                     Do not create the file any other way. Then reply with one word.";

#[tokio::test]
#[ignore]
async fn claude_allow() {
    let dir = scratch("claude-allow");
    let out = run(
        "claude",
        model("haiku"),
        &dir,
        TOUCH,
        None,
        Answer::Pick("allow_once"),
    )
    .await;
    assert_eq!(out.end, End::Completed);
    assert!(!out.approvals.is_empty());
    assert!(dir.join("made.txt").exists());
}

#[tokio::test]
#[ignore]
async fn claude_deny() {
    let dir = scratch("claude-deny");
    let out = run(
        "claude",
        model("haiku"),
        &dir,
        TOUCH,
        None,
        Answer::Pick("reject_once"),
    )
    .await;
    assert_eq!(out.end, End::Completed);
    assert!(!out.approvals.is_empty());
    assert!(!dir.join("made.txt").exists());
    assert!(
        out.statuses.contains(&ToolCallStatus::Denied),
        "{:?}",
        out.statuses
    );
}

#[tokio::test]
#[ignore]
async fn claude_stop_while_waiting() {
    let dir = scratch("claude-stop");
    let out = run("claude", model("haiku"), &dir, TOUCH, None, Answer::Stop).await;
    assert_eq!(out.end, End::Cancelled);
    assert!(!dir.join("made.txt").exists());
}

#[tokio::test]
#[ignore]
async fn codex_allow_then_resume() {
    let dir = scratch("codex-allow");
    let out = run(
        "codex",
        Opts::default(),
        &dir,
        TOUCH,
        None,
        Answer::Pick("allow_once"),
    )
    .await;
    assert_eq!(out.end, End::Completed);
    assert!(!out.approvals.is_empty());
    assert!(dir.join("made.txt").exists());

    let thread = out.external_id.expect("thread id");
    let again = run(
        "codex",
        Opts::default(),
        &dir,
        "What file did you just create? Answer with the file name only, without using tools.",
        Some(thread.clone()),
        Answer::Pick("allow_once"),
    )
    .await;
    assert_eq!(again.end, End::Completed);
    assert_eq!(again.external_id.as_deref(), Some(thread.as_str()));
}

#[tokio::test]
#[ignore]
async fn codex_deny() {
    let dir = scratch("codex-deny");
    let out = run(
        "codex",
        Opts::default(),
        &dir,
        TOUCH,
        None,
        Answer::Pick("reject_once"),
    )
    .await;
    assert_eq!(out.end, End::Completed);
    assert!(!out.approvals.is_empty());
    assert!(!dir.join("made.txt").exists());
    assert!(
        out.statuses.contains(&ToolCallStatus::Denied),
        "{:?}",
        out.statuses
    );
}

#[tokio::test]
#[ignore]
async fn codex_stop_while_waiting() {
    let dir = scratch("codex-stop");
    let out = run("codex", Opts::default(), &dir, TOUCH, None, Answer::Stop).await;
    assert_eq!(out.end, End::Cancelled);
    assert!(!dir.join("made.txt").exists());
}

#[tokio::test]
#[ignore]
async fn codex_missing_thread_starts_fresh() {
    let dir = scratch("codex-missing");
    let out = run(
        "codex",
        Opts::default(),
        &dir,
        "Reply with one word, without using tools.",
        Some("00000000-0000-0000-0000-000000000000".into()),
        Answer::Pick("allow_once"),
    )
    .await;
    assert_eq!(out.end, End::Completed);
    assert_ne!(
        out.external_id.as_deref(),
        Some("00000000-0000-0000-0000-000000000000")
    );
}

#[tokio::test]
#[ignore]
async fn claude_auto_approve_runs_bash_without_asking() {
    let dir = scratch("claude-auto");
    let opts = Opts {
        auto_approve: true,
        ..model("haiku")
    };
    let out = run(
        "claude",
        opts,
        &dir,
        TOUCH,
        None,
        Answer::Pick("reject_once"),
    )
    .await;
    assert_eq!(out.end, End::Completed);
    assert!(out.approvals.is_empty());
    assert!(dir.join("made.txt").exists());
}

#[tokio::test]
#[ignore]
async fn codex_auto_approve_runs_without_asking() {
    let dir = scratch("codex-auto");
    let opts = Opts {
        auto_approve: true,
        ..Opts::default()
    };
    let out = run(
        "codex",
        opts,
        &dir,
        TOUCH,
        None,
        Answer::Pick("reject_once"),
    )
    .await;
    assert_eq!(out.end, End::Completed);
    assert!(out.approvals.is_empty());
    assert!(dir.join("made.txt").exists());
}

/// Set `SAME_CODEX_MODEL` / `SAME_CODEX_EFFORT` to values from model/list.
#[tokio::test]
#[ignore]
async fn codex_with_model_and_effort() {
    let (Ok(m), Ok(e)) = (
        std::env::var("SAME_CODEX_MODEL"),
        std::env::var("SAME_CODEX_EFFORT"),
    ) else {
        return;
    };
    let dir = scratch("codex-model");
    let opts = Opts {
        model: Some(Box::leak(m.into_boxed_str())),
        effort: Some(Box::leak(e.into_boxed_str())),
        auto_approve: false,
    };
    let out = run(
        "codex",
        opts,
        &dir,
        "Reply with one word, without using tools.",
        None,
        Answer::Pick("allow_once"),
    )
    .await;
    assert_eq!(out.end, End::Completed);
}

/// Set `SAME_CODEX_EXEC_THREAD` to a thread id printed by `codex exec --json`.
#[tokio::test]
#[ignore]
async fn codex_resumes_exec_thread() {
    let Ok(thread) = std::env::var("SAME_CODEX_EXEC_THREAD") else {
        return;
    };
    let dir = scratch("codex-exec-resume");
    let out = run(
        "codex",
        Opts::default(),
        &dir,
        "Reply with one word, without using tools.",
        Some(thread.clone()),
        Answer::Pick("allow_once"),
    )
    .await;
    assert_eq!(out.end, End::Completed);
    assert_eq!(out.external_id.as_deref(), Some(thread.as_str()));
}

#[tokio::test]
#[ignore]
async fn codex_deny_and_stop_cancels() {
    let dir = scratch("codex-deny-stop");
    let out = run(
        "codex",
        Opts::default(),
        &dir,
        TOUCH,
        None,
        Answer::Pick("reject_always"),
    )
    .await;
    assert_eq!(out.end, End::Cancelled);
    assert!(!dir.join("made.txt").exists());
}

/// "Always allow" writes Claude's suggested rule to the project's
/// `.claude/settings.local.json` (here a scratch folder).
#[tokio::test]
#[ignore]
async fn claude_always_allow_saves_rule() {
    let dir = scratch("claude-always");
    let out = run(
        "claude",
        model("haiku"),
        &dir,
        TOUCH,
        None,
        Answer::Pick("allow_always"),
    )
    .await;
    assert_eq!(out.end, End::Completed);
    assert!(dir.join("made.txt").exists());
    let settings = std::fs::read_to_string(dir.join(".claude/settings.local.json"))
        .expect("settings.local.json written");
    assert!(settings.contains("Bash"), "{settings}");
}
