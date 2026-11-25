//! Permission prompts raised by the CLI adapters (Claude Code's stdio
//! permission prompt tool, Codex app-server approval requests), routed to the
//! same approval UI as ACP's `session/request_permission`.

use same_acp::{PermissionDecision, PermissionResponder};
use same_core::{
    ApprovalMode, EventBus, PermissionOption, PermissionRequest, SameEvent, SessionStatus,
    UserInputRequest, UserQuestion,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{oneshot, Mutex};

pub(crate) type Responders = Arc<Mutex<HashMap<String, PermissionResponder>>>;

/// Answers to a question card, per question in order (selected option
/// labels and any free text); `None` when dismissed.
pub type Answers = Option<Vec<Vec<String>>>;
pub(crate) type InputResponders = Arc<Mutex<HashMap<String, oneshot::Sender<Answers>>>>;

/// Each session's approval mode, read when a prompt arrives so a change
/// applies mid-turn.
pub type ModeStore = Arc<std::sync::Mutex<HashMap<String, ApprovalMode>>>;

pub fn mode_of(modes: &ModeStore, session_id: &str) -> ApprovalMode {
    modes
        .lock()
        .unwrap()
        .get(session_id)
        .copied()
        .unwrap_or_default()
}

/// When SAME may answer a prompt itself instead of asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoAllow {
    /// The agent applies the mode itself, so whatever it still asks (e.g.
    /// Claude Code's safety checks) is shown to the user.
    Never,
    /// Allowed once in full access.
    InFullAccess,
    /// A file edit: allowed once in full access and in accept-edits.
    Edit,
}

impl AutoAllow {
    pub fn allows(self, mode: ApprovalMode) -> bool {
        match self {
            AutoAllow::Never => false,
            AutoAllow::InFullAccess => mode == ApprovalMode::FullAccess,
            AutoAllow::Edit => matches!(mode, ApprovalMode::FullAccess | ApprovalMode::AcceptEdits),
        }
    }
}

/// What an agent wants approved.
#[derive(Debug, Clone)]
pub struct ApprovalPrompt {
    pub tool_call_id: Option<String>,
    pub title: String,
    pub description: String,
    /// Option kinds follow ACP: `allow_once`, `allow_always`, `reject_once`, ...
    pub options: Vec<PermissionOption>,
    /// A unified diff of the change, for file edits.
    pub diff: Option<String>,
    pub auto: AutoAllow,
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Asks the user (or the live auto-approve policy) to decide a prompt.
#[derive(Clone)]
pub struct PermissionGate {
    events: EventBus,
    responders: Responders,
    input_responders: InputResponders,
    pending: Arc<std::sync::Mutex<HashMap<String, usize>>>,
    modes: ModeStore,
}

impl PermissionGate {
    pub(crate) fn new(
        events: EventBus,
        responders: Responders,
        input_responders: InputResponders,
        modes: ModeStore,
    ) -> Self {
        Self {
            events,
            responders,
            input_responders,
            pending: Arc::new(std::sync::Mutex::new(HashMap::new())),
            modes,
        }
    }

    pub fn mode(&self, session_id: &str) -> ApprovalMode {
        mode_of(&self.modes, session_id)
    }

    /// Asks the user the agent's questions. Not subject to the approval mode:
    /// even in full access the agent needs the answers.
    pub async fn ask_user(&self, session_id: &str, questions: Vec<UserQuestion>) -> Answers {
        let request_id = format!(
            "input-{session_id}-{}",
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        );
        let (tx, rx) = oneshot::channel();
        self.input_responders
            .lock()
            .await
            .insert(request_id.clone(), tx);
        let mut guard = InputGuard {
            events: self.events.clone(),
            responders: self.input_responders.clone(),
            pending: self.pending.clone(),
            session_id: session_id.to_string(),
            request_id: request_id.clone(),
            resolved: false,
        };
        {
            let mut pending = self.pending.lock().unwrap();
            *pending.entry(session_id.to_string()).or_insert(0) += 1;
            self.events.publish(SameEvent::SessionStatusChanged {
                session_id: session_id.to_string(),
                status: SessionStatus::WaitingForApproval,
            });
        }
        self.events.publish(SameEvent::UserInputRequested {
            request: UserInputRequest {
                request_id,
                session_id: session_id.to_string(),
                questions,
            },
        });
        let received = rx.await;
        let answered = received.is_ok();
        let answers = received.ok().flatten();
        guard.resolved = true;
        {
            let mut map = self.pending.lock().unwrap();
            let remaining = decrement_pending_locked(&mut map, session_id);
            if remaining == 0 && answered {
                self.events.publish(SameEvent::SessionStatusChanged {
                    session_id: session_id.to_string(),
                    status: SessionStatus::Running,
                });
            }
        }
        answers
    }

    /// Returns the chosen option id, or `None` when the prompt was declined
    /// without a choice (the run was stopped, or the prompt was dropped).
    ///
    /// When the session's mode allows it (see [`AutoAllow`]), the first
    /// single-use allow option is chosen without asking.
    pub async fn ask(&self, session_id: &str, prompt: ApprovalPrompt) -> Option<String> {
        if prompt.auto.allows(self.mode(session_id)) {
            if let Some(opt) = prompt.options.iter().find(|o| o.kind == "allow_once") {
                return Some(opt.id.clone());
            }
        }

        let request_id = format!(
            "cli-{session_id}-{}",
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        );
        let (responder, rx) = PermissionResponder::new();
        self.responders
            .lock()
            .await
            .insert(request_id.clone(), responder);
        // Removes the responder and withdraws the prompt if this future is
        // dropped before an answer (the turn ended or was cancelled).
        let mut guard = ResponderGuard {
            events: self.events.clone(),
            responders: self.responders.clone(),
            pending: self.pending.clone(),
            session_id: session_id.to_string(),
            request_id: request_id.clone(),
            resolved: false,
        };

        {
            let mut pending = self.pending.lock().unwrap();
            *pending.entry(session_id.to_string()).or_insert(0) += 1;
            self.events.publish(SameEvent::SessionStatusChanged {
                session_id: session_id.to_string(),
                status: SessionStatus::WaitingForApproval,
            });
        }
        self.events.publish(SameEvent::ApprovalRequested {
            request: PermissionRequest {
                request_id,
                session_id: session_id.to_string(),
                tool_call_id: prompt.tool_call_id,
                title: prompt.title,
                description: prompt.description,
                options: prompt.options,
                diff: prompt.diff,
            },
        });

        let result = rx.await;
        guard.resolved = true;
        match result {
            Ok(PermissionDecision::Selected { option_id }) => {
                let mut map = self.pending.lock().unwrap();
                let remaining = decrement_pending_locked(&mut map, session_id);
                if remaining == 0 {
                    self.events.publish(SameEvent::SessionStatusChanged {
                        session_id: session_id.to_string(),
                        status: SessionStatus::Running,
                    });
                }
                Some(option_id)
            }
            _ => {
                let mut map = self.pending.lock().unwrap();
                decrement_pending_locked(&mut map, session_id);
                None
            }
        }
    }
}

fn decrement_pending_locked(map: &mut HashMap<String, usize>, session_id: &str) -> usize {
    if let Some(count) = map.get_mut(session_id) {
        *count = count.saturating_sub(1);
        if *count == 0 {
            map.remove(session_id);
            return 0;
        }
        return *count;
    }
    0
}

struct ResponderGuard {
    events: EventBus,
    responders: Responders,
    pending: Arc<std::sync::Mutex<HashMap<String, usize>>>,
    session_id: String,
    request_id: String,
    resolved: bool,
}

impl Drop for ResponderGuard {
    fn drop(&mut self) {
        let responders = self.responders.clone();
        let id = std::mem::take(&mut self.request_id);
        if !self.resolved {
            let sid = std::mem::take(&mut self.session_id);
            let mut map = self.pending.lock().unwrap();
            let remaining = decrement_pending_locked(&mut map, &sid);
            self.events.publish(SameEvent::ApprovalWithdrawn {
                session_id: sid,
                request_id: id.clone(),
                remaining,
            });
        }
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                responders.lock().await.remove(&id);
            });
        }
    }
}

struct InputGuard {
    events: EventBus,
    responders: InputResponders,
    pending: Arc<std::sync::Mutex<HashMap<String, usize>>>,
    session_id: String,
    request_id: String,
    resolved: bool,
}

impl Drop for InputGuard {
    fn drop(&mut self) {
        let responders = self.responders.clone();
        let id = std::mem::take(&mut self.request_id);
        if !self.resolved {
            let sid = std::mem::take(&mut self.session_id);
            {
                let mut map = self.pending.lock().unwrap();
                decrement_pending_locked(&mut map, &sid);
            }
            self.events.publish(SameEvent::UserInputWithdrawn {
                session_id: sid,
                request_id: id.clone(),
            });
        }
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                responders.lock().await.remove(&id);
            });
        }
    }
}

pub(crate) fn option(id: &str, name: impl Into<String>, kind: &str) -> PermissionOption {
    PermissionOption {
        id: id.to_string(),
        name: name.into(),
        kind: kind.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt(auto: AutoAllow) -> ApprovalPrompt {
        ApprovalPrompt {
            tool_call_id: None,
            title: "Run".into(),
            description: String::new(),
            options: vec![
                option("always", "Always", "allow_always"),
                option("once", "Allow", "allow_once"),
                option("no", "Deny", "reject_once"),
            ],
            diff: None,
            auto,
        }
    }

    fn gate_with(events: EventBus, responders: Responders, mode: ApprovalMode) -> PermissionGate {
        let modes = ModeStore::default();
        modes.lock().unwrap().insert("s".into(), mode);
        PermissionGate::new(events, responders, InputResponders::default(), modes)
    }

    #[test]
    fn auto_allow_follows_mode() {
        use ApprovalMode::*;
        for (auto, allowed) in [
            (AutoAllow::Never, vec![]),
            (AutoAllow::InFullAccess, vec![FullAccess]),
            (AutoAllow::Edit, vec![AcceptEdits, FullAccess]),
        ] {
            for mode in ApprovalMode::ALL {
                assert_eq!(
                    auto.allows(mode),
                    allowed.contains(&mode),
                    "{auto:?} {mode:?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn full_access_picks_single_use_allow_without_asking() {
        let events = EventBus::new(16);
        let mut rx = events.subscribe();
        let gate = gate_with(events, Responders::default(), ApprovalMode::FullAccess);
        assert_eq!(
            gate.ask("s", prompt(AutoAllow::InFullAccess))
                .await
                .as_deref(),
            Some("once")
        );
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn asks_and_returns_choice_then_cleans_up() {
        let events = EventBus::new(16);
        let mut rx = events.subscribe();
        let responders = Responders::default();
        // The agent enforces the mode itself, so even full access asks.
        let gate = gate_with(events, responders.clone(), ApprovalMode::FullAccess);
        let task = tokio::spawn(async move { gate.ask("s", prompt(AutoAllow::Never)).await });
        let request_id = loop {
            if let SameEvent::ApprovalRequested { request } = rx.recv().await.unwrap() {
                break request.request_id;
            }
        };
        responders
            .lock()
            .await
            .remove(&request_id)
            .unwrap()
            .respond(PermissionDecision::Selected {
                option_id: "no".into(),
            });
        assert_eq!(task.await.unwrap().as_deref(), Some("no"));
        tokio::task::yield_now().await;
        assert!(responders.lock().await.is_empty());
    }

    #[tokio::test]
    async fn dropped_prompt_removes_its_responder() {
        let responders = Responders::default();
        let gate = gate_with(EventBus::new(16), responders.clone(), ApprovalMode::Ask);
        let task = tokio::spawn(async move { gate.ask("s", prompt(AutoAllow::Edit)).await });
        while responders.lock().await.is_empty() {
            tokio::task::yield_now().await;
        }
        task.abort();
        let _ = task.await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert!(responders.lock().await.is_empty());
    }

    #[tokio::test]
    async fn questions_return_answers_and_withdraw_when_dropped() {
        let events = EventBus::new(16);
        let mut rx = events.subscribe();
        let inputs = InputResponders::default();
        let gate = PermissionGate::new(
            events,
            Responders::default(),
            inputs.clone(),
            ModeStore::default(),
        );
        let q = UserQuestion {
            id: "q".into(),
            header: "H".into(),
            question: "Which?".into(),
            options: vec![],
            multi_select: false,
            allow_other: true,
            secret: false,
        };
        let g = gate.clone();
        let qq = q.clone();
        let task = tokio::spawn(async move { g.ask_user("s", vec![qq]).await });
        let rid = loop {
            if let SameEvent::UserInputRequested { request } = rx.recv().await.unwrap() {
                break request.request_id;
            }
        };
        let tx = inputs.lock().await.remove(&rid).unwrap();
        tx.send(Some(vec![vec!["A".into()]])).unwrap();
        assert_eq!(task.await.unwrap(), Some(vec![vec!["A".to_string()]]));

        let task = tokio::spawn(async move { gate.ask_user("s", vec![q]).await });
        while inputs.lock().await.is_empty() {
            tokio::task::yield_now().await;
        }
        task.abort();
        let _ = task.await;
        let withdrawn = loop {
            match rx.recv().await.unwrap() {
                SameEvent::UserInputWithdrawn { .. } => break true,
                _ => continue,
            }
        };
        assert!(withdrawn);
    }
}
