//! End-to-end approval flow against the real `claude` and `codex` CLIs.
//! They cost tokens and need logged-in CLIs, so they are ignored by default:
//! `cargo test -p same-agent --test cli_approvals -- --ignored --test-threads=1`

use same_agent::{AgentManager, ExecPreference, PromptRequest};
use same_core::config::default_providers;
use same_core::{
    ApprovalMode, EventBus, PermissionRequest, SameEvent, ToolCallStatus, UserInputRequest,
};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

#[derive(Clone, Copy)]
enum Answer {
    /// Choose the first option of this kind.
    Pick(&'static str),
    /// Stop the run while the prompt is open (as the Stop button does).
    Stop,
    /// Switch the session to full access, then allow once.
    SwitchToFullAccess,
}

#[derive(Debug, PartialEq)]
enum End {
    Completed,
    Cancelled,
    Failed(String),
}

struct Outcome {
    approvals: Vec<PermissionRequest>,
    questions: Vec<UserInputRequest>,
    plans: Vec<String>,
    end: End,
    external_id: Option<String>,
    statuses: Vec<ToolCallStatus>,
    elapsed: Duration,
}

/// A SAME session on one manager, so turns reuse the agent process.
struct Session {
    mgr: AgentManager,
    rx: broadcast::Receiver<SameEvent>,
    provider_id: &'static str,
    sid: String,
    cwd: PathBuf,
    external_id: Option<String>,
    model: Option<&'static str>,
    effort: Option<&'static str>,
    mode: ApprovalMode,
}

impl Session {
    fn new(provider_id: &'static str, name: &str) -> Self {
        let events = EventBus::new(4096);
        let rx = events.subscribe();
        let cwd = scratch(name);
        Self {
            mgr: AgentManager::new(events, tokio::runtime::Handle::current()),
            rx,
            provider_id,
            sid: format!("e2e-{name}"),
            cwd,
            external_id: None,
            model: (provider_id == "claude").then_some("haiku"),
            effort: None,
            mode: ApprovalMode::Ask,
        }
    }

    async fn turn(&mut self, prompt: &str, answer: Answer) -> Outcome {
        let provider = default_providers()
            .into_iter()
            .find(|p| p.id == self.provider_id)
            .unwrap();
        let started = Instant::now();
        self.mgr.start_prompt(PromptRequest {
            session_id: self.sid.clone(),
            external_session_id: self.external_id.clone(),
            provider,
            model: self.model.map(str::to_string),
            effort: self.effort.map(str::to_string),
            cwd: self.cwd.clone(),
            prompt: prompt.to_string(),
            attachments: vec![],
            exec: ExecPreference::Auto,
            mode: self.mode,
            read_only: false,
            fallback_handoff_prompt: None,
            skill_delivery: None,
            skill_sync: None,
        });

        let mut out = Outcome {
            approvals: vec![],
            questions: vec![],
            plans: vec![],
            end: End::Failed("timed out".into()),
            external_id: None,
            statuses: vec![],
            elapsed: Duration::ZERO,
        };
        let sid = self.sid.clone();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(240);
        loop {
            let ev = match tokio::time::timeout_at(deadline, self.rx.recv()).await {
                Ok(Ok(ev)) => ev,
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
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
                                .unwrap_or_else(|| {
                                    panic!("no {kind} option in {:?}", request.options)
                                });
                            self.mgr
                                .respond_permission(&request.request_id, Some(opt.id.clone()))
                                .await;
                        }
                        // The app's order: answer the prompt, then cancel.
                        Answer::Stop => {
                            self.mgr.respond_permission(&request.request_id, None).await;
                            self.mgr.cancel_session(&sid).await;
                        }
                        Answer::SwitchToFullAccess => {
                            self.mgr.set_session_mode(&sid, ApprovalMode::FullAccess);
                            tokio::time::sleep(Duration::from_millis(300)).await;
                            let opt = request
                                .options
                                .iter()
                                .find(|o| o.kind == "allow_once")
                                .unwrap();
                            self.mgr
                                .respond_permission(&request.request_id, Some(opt.id.clone()))
                                .await;
                        }
                    }
                }
                SameEvent::UserInputRequested { request } if request.session_id == sid => {
                    // Pick each question's first option.
                    let answers = request
                        .questions
                        .iter()
                        .map(|q| {
                            q.options
                                .first()
                                .map(|o| vec![o.label.clone()])
                                .unwrap_or_else(|| vec!["yes".to_string()])
                        })
                        .collect();
                    out.questions.push(request.clone());
                    self.mgr
                        .respond_user_input(&request.request_id, Some(answers))
                        .await;
                }
                SameEvent::PlanProposed { session_id, plan } if session_id == sid => {
                    out.plans.push(plan)
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
        out.elapsed = started.elapsed();
        if out.external_id.is_some() {
            self.external_id = out.external_id.clone();
        }
        out
    }

    /// A path next to (outside) the workspace, which Codex's workspace-write
    /// sandbox does not cover, so writing it needs approval.
    fn outside(&self, file: &str) -> PathBuf {
        let dir = self.cwd.with_extension("outside");
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(file)
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("same-approvals-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

const TOUCH: &str = "Run exactly this shell command with your shell tool: `touch made.txt`. \
                     Do not create the file any other way. Then reply with one word.";

fn touch_outside(path: &std::path::Path) -> String {
    format!(
        "Run exactly `touch {}` with your shell tool. It writes outside the workspace, so \
         request escalated permissions for it. Do not create the file any other way. \
         Then reply with one word.",
        path.display()
    )
}

const ONE_WORD: &str = "Reply with one word, without using tools.";

// ---- Claude Code ----

#[tokio::test]
#[ignore]
async fn claude_allow() {
    let mut s = Session::new("claude", "claude-allow");
    let out = s.turn(TOUCH, Answer::Pick("allow_once")).await;
    assert_eq!(out.end, End::Completed);
    assert!(!out.approvals.is_empty());
    assert!(s.cwd.join("made.txt").exists());
}

#[tokio::test]
#[ignore]
async fn claude_deny() {
    let mut s = Session::new("claude", "claude-deny");
    let out = s.turn(TOUCH, Answer::Pick("reject_once")).await;
    assert_eq!(out.end, End::Completed);
    assert!(!out.approvals.is_empty());
    assert!(!s.cwd.join("made.txt").exists());
    assert!(
        out.statuses.contains(&ToolCallStatus::Denied),
        "{:?}",
        out.statuses
    );
}

/// Stopping while a prompt is open ends the turn, and the same process takes
/// the next one.
#[tokio::test]
#[ignore]
async fn claude_stop_while_waiting_then_continue() {
    let mut s = Session::new("claude", "claude-stop");
    let out = s.turn(TOUCH, Answer::Stop).await;
    assert_eq!(out.end, End::Cancelled);
    assert!(!s.cwd.join("made.txt").exists());
    let next = s.turn(ONE_WORD, Answer::Pick("allow_once")).await;
    assert_eq!(next.end, End::Completed);
}

/// The second turn reuses the process: faster, and a session grant from the
/// first turn still holds.
#[tokio::test]
#[ignore]
async fn claude_process_persists_with_session_grant() {
    let mut s = Session::new("claude", "claude-persist");
    let first = s.turn(TOUCH, Answer::Pick("allow_always")).await;
    assert_eq!(first.end, End::Completed);
    // "allow_always" kind: the first such option is "Allow for this session".
    assert_eq!(first.approvals[0].options[1].id, "allow_session");
    let second = s
        .turn(
            "Run exactly `touch made.txt` again with your shell tool. Then reply with one word.",
            Answer::Pick("reject_once"),
        )
        .await;
    assert_eq!(second.end, End::Completed);
    assert!(
        second.approvals.is_empty(),
        "asked again: {:?}",
        second.approvals
    );
    assert!(
        second.elapsed < first.elapsed,
        "{:?} vs {:?}",
        second.elapsed,
        first.elapsed
    );
    assert!(!s.cwd.join(".claude/settings.local.json").exists());
}

#[tokio::test]
#[ignore]
async fn claude_always_allow_saves_rule() {
    let mut s = Session::new("claude", "claude-always");
    let out = s.turn(TOUCH, Answer::Pick("allow_always")).await;
    assert_eq!(out.end, End::Completed);
    let opt = &out.approvals[0].options;
    assert!(opt.iter().any(|o| o.id == "allow_always"));
    assert!(s.cwd.join("made.txt").exists());
}

#[tokio::test]
#[ignore]
async fn claude_full_access_runs_bash_without_asking() {
    let mut s = Session::new("claude", "claude-full");
    s.mode = ApprovalMode::FullAccess;
    let out = s.turn(TOUCH, Answer::Pick("reject_once")).await;
    assert_eq!(out.end, End::Completed);
    assert!(out.approvals.is_empty());
    assert!(s.cwd.join("made.txt").exists());
}

/// Plan mode: a question card, then a plan card; nothing is written. Then
/// implementing in ask mode on the same process.
#[tokio::test]
#[ignore]
async fn claude_plan_question_then_implement() {
    let mut s = Session::new("claude", "claude-plan");
    s.mode = ApprovalMode::Plan;
    let plan = s
        .turn(
            "I want a file notes.txt. First use AskUserQuestion to ask me whether its content \
             should be 'alpha' or 'beta' (two options). Then present a tiny plan with ExitPlanMode.",
            Answer::Pick("allow_once"),
        )
        .await;
    assert_eq!(plan.end, End::Completed);
    assert_eq!(plan.questions.len(), 1, "no question card");
    assert_eq!(plan.plans.len(), 1, "no plan card");
    assert!(!s.cwd.join("notes.txt").exists());

    s.mode = ApprovalMode::AcceptEdits;
    let done = s
        .turn(
            "Implement the plan now: create notes.txt with the Write tool.",
            Answer::Pick("allow_once"),
        )
        .await;
    assert_eq!(done.end, End::Completed);
    let content = std::fs::read_to_string(s.cwd.join("notes.txt")).unwrap();
    assert_eq!(
        content.trim(),
        plan.questions[0].questions[0].options[0].label
    );
}

/// A mode change reaches a turn already running.
#[tokio::test]
#[ignore]
async fn claude_mode_switch_mid_turn() {
    let mut s = Session::new("claude", "claude-switch");
    let out = s
        .turn(
            "Run `touch a.txt` with Bash. After it finishes, run `touch b.txt` with Bash as a \
             separate tool call. Then reply with one word.",
            Answer::SwitchToFullAccess,
        )
        .await;
    assert_eq!(out.end, End::Completed);
    assert_eq!(out.approvals.len(), 1, "{:?}", out.approvals);
    assert!(s.cwd.join("a.txt").exists() && s.cwd.join("b.txt").exists());
}

// ---- Codex ----

#[tokio::test]
#[ignore]
async fn codex_allow_then_resume_in_new_process() {
    let mut s = Session::new("codex", "codex-allow");
    let target = s.outside("made.txt");
    let out = s
        .turn(&touch_outside(&target), Answer::Pick("allow_once"))
        .await;
    assert_eq!(out.end, End::Completed);
    assert!(!out.approvals.is_empty());
    assert!(target.exists());

    // A new manager (as after an app restart) resumes the thread.
    s.mgr.shutdown_all().await;
    let thread = out.external_id.expect("thread id");
    let mut again = Session::new("codex", "codex-allow-2");
    again.cwd = s.cwd.clone();
    again.external_id = Some(thread.clone());
    let out = again.turn(ONE_WORD, Answer::Pick("allow_once")).await;
    assert_eq!(out.end, End::Completed);
    assert_eq!(out.external_id.as_deref(), Some(thread.as_str()));
}

#[tokio::test]
#[ignore]
async fn codex_workspace_writes_need_no_approval() {
    let mut s = Session::new("codex", "codex-inside");
    let out = s.turn(TOUCH, Answer::Pick("reject_once")).await;
    assert_eq!(out.end, End::Completed);
    assert!(out.approvals.is_empty());
    assert!(s.cwd.join("made.txt").exists());
}

#[tokio::test]
#[ignore]
async fn codex_deny() {
    let mut s = Session::new("codex", "codex-deny");
    let target = s.outside("made.txt");
    let out = s
        .turn(&touch_outside(&target), Answer::Pick("reject_once"))
        .await;
    assert_eq!(out.end, End::Completed);
    assert!(!out.approvals.is_empty());
    assert!(!target.exists());
    assert!(
        out.statuses.contains(&ToolCallStatus::Denied),
        "{:?}",
        out.statuses
    );
}

#[tokio::test]
#[ignore]
async fn codex_stop_while_waiting_then_continue() {
    let mut s = Session::new("codex", "codex-stop");
    let target = s.outside("made.txt");
    let out = s.turn(&touch_outside(&target), Answer::Stop).await;
    assert_eq!(out.end, End::Cancelled);
    assert!(!target.exists());
    let next = s.turn(ONE_WORD, Answer::Pick("allow_once")).await;
    assert_eq!(next.end, End::Completed);
}

#[tokio::test]
#[ignore]
async fn codex_deny_and_stop_cancels() {
    let mut s = Session::new("codex", "codex-deny-stop");
    let target = s.outside("made.txt");
    let out = s
        .turn(&touch_outside(&target), Answer::Pick("reject_always"))
        .await;
    assert_eq!(out.end, End::Cancelled);
    assert!(!target.exists());
}

/// A session grant from one turn covers the next on the same process.
#[tokio::test]
#[ignore]
async fn codex_session_grant_persists() {
    let mut s = Session::new("codex", "codex-grant");
    let target = s.outside("made.txt");
    let first = s
        .turn(&touch_outside(&target), Answer::Pick("allow_always"))
        .await;
    assert_eq!(first.end, End::Completed);
    assert_eq!(first.approvals[0].options[1].name, "Allow for this session");
    let second = s
        .turn(&touch_outside(&target), Answer::Pick("reject_once"))
        .await;
    assert_eq!(second.end, End::Completed);
    assert!(
        second.approvals.is_empty(),
        "asked again: {:?}",
        second.approvals
    );
}

#[tokio::test]
#[ignore]
async fn codex_full_access_runs_without_asking() {
    let mut s = Session::new("codex", "codex-full");
    s.mode = ApprovalMode::FullAccess;
    let target = s.outside("made.txt");
    // No escalation request: under the `never` policy Codex refuses those,
    // and with the sandbox off none is needed.
    let prompt = format!(
        "Run exactly `touch {}` with your shell tool. Then reply with one word.",
        target.display()
    );
    let out = s.turn(&prompt, Answer::Pick("reject_once")).await;
    assert_eq!(out.end, End::Completed);
    assert!(out.approvals.is_empty());
    assert!(target.exists());
}

#[tokio::test]
#[ignore]
async fn codex_plan_question_then_plan() {
    let mut s = Session::new("codex", "codex-plan");
    s.mode = ApprovalMode::Plan;
    let out = s
        .turn(
            "Plan how to add a README.md to this folder. Before planning, use \
             request_user_input to ask whether it should be short or long. Then give the plan.",
            Answer::Pick("allow_once"),
        )
        .await;
    assert_eq!(out.end, End::Completed);
    assert_eq!(out.questions.len(), 1, "no question card");
    assert_eq!(out.plans.len(), 1, "no plan card");
    assert!(!s.cwd.join("README.md").exists());
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
    let mut s = Session::new("codex", "codex-model");
    s.model = Some(Box::leak(m.into_boxed_str()));
    s.effort = Some(Box::leak(e.into_boxed_str()));
    let out = s.turn(ONE_WORD, Answer::Pick("allow_once")).await;
    assert_eq!(out.end, End::Completed);
    // Plan mode names the model in its collaboration mode.
    s.mode = ApprovalMode::Plan;
    let out = s.turn(ONE_WORD, Answer::Pick("allow_once")).await;
    assert_eq!(out.end, End::Completed);
}

#[tokio::test]
#[ignore]
async fn codex_missing_thread_starts_fresh() {
    let mut s = Session::new("codex", "codex-missing");
    s.external_id = Some("00000000-0000-0000-0000-000000000000".into());
    let out = s.turn(ONE_WORD, Answer::Pick("allow_once")).await;
    assert_eq!(out.end, End::Completed);
    assert_ne!(
        out.external_id.as_deref(),
        Some("00000000-0000-0000-0000-000000000000")
    );
}

#[tokio::test]
#[ignore]
async fn claude_missing_conversation_starts_fresh() {
    let mut s = Session::new("claude", "claude-missing");
    s.external_id = Some("11111111-1111-1111-1111-111111111111".into());
    let out = s.turn(ONE_WORD, Answer::Pick("allow_once")).await;
    assert_eq!(out.end, End::Completed);
    assert_ne!(
        out.external_id.as_deref(),
        Some("11111111-1111-1111-1111-111111111111")
    );
}
