use crate::models::{
    HandoffMode, ModelInfo, PermissionRequest, ProviderCapabilities, SessionStatus,
    SlashCommandInfo, ToolCallRecord, UserInputRequest,
};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SameEvent {
    SessionStarted {
        session_id: String,
        provider_id: String,
    },
    SessionStatusChanged {
        session_id: String,
        status: SessionStatus,
    },
    ExternalSessionBound {
        session_id: String,
        external_session_id: String,
    },
    /// A provider-specific native session was created or resumed.
    ProviderSessionBound {
        session_id: String,
        provider_id: String,
        external_session_id: String,
    },
    /// Context handoff was prepared and applied when switching or resuming.
    ContextHandoffApplied {
        session_id: String,
        from_provider_id: Option<String>,
        to_provider_id: String,
        mode: HandoffMode,
        is_delta: bool,
    },
    /// Resuming a native provider session failed and fell back to a fresh
    /// session with context handoff.
    NativeSessionFallback {
        session_id: String,
        provider_id: String,
        reason: String,
    },
    MessageDelta {
        session_id: String,
        text: String,
    },
    ThinkingDelta {
        session_id: String,
        text: String,
    },
    ToolCallStarted {
        session_id: String,
        tool_call: ToolCallRecord,
    },
    ToolCallUpdated {
        session_id: String,
        tool_call: ToolCallRecord,
    },
    ApprovalRequested {
        request: PermissionRequest,
    },
    ApprovalWithdrawn {
        session_id: String,
        request_id: String,
        remaining: usize,
    },
    /// The agent asks the user questions and waits for the answers.
    UserInputRequested {
        request: UserInputRequest,
    },
    /// The questions no longer need answers (the turn ended or was stopped).
    UserInputWithdrawn {
        session_id: String,
        request_id: String,
    },
    /// The agent proposed a plan (plan mode) and stopped to wait for the user.
    PlanProposed {
        session_id: String,
        plan: String,
    },
    SlashCommandsUpdated {
        session_id: String,
        commands: Vec<SlashCommandInfo>,
    },
    /// The models an agent offers for a session, and the one in use.
    ModelsUpdated {
        session_id: String,
        provider_id: String,
        models: Vec<ModelInfo>,
        current: Option<String>,
    },
    SessionCompleted {
        session_id: String,
        stop_reason: Option<String>,
    },
    SessionFailed {
        session_id: String,
        error: String,
    },
    SessionCancelled {
        session_id: String,
    },
    AgentLog {
        session_id: Option<String>,
        provider_id: String,
        line: String,
    },
    ProviderCapabilitiesUpdated {
        provider_id: String,
        capabilities: ProviderCapabilities,
    },
    SkillCreated {
        skill_id: String,
    },
    SkillUpdated {
        skill_id: String,
    },
    SkillDeleted {
        skill_id: String,
    },
    SkillImported {
        skill_id: String,
    },
    SkillEnabled {
        skill_id: String,
    },
    SkillDisabled {
        skill_id: String,
    },
    SkillAssignmentChanged {
        skill_id: String,
        project_id: String,
        enabled: Option<bool>,
    },
    SkillSyncStarted {
        provider_id: String,
        project_id: Option<String>,
    },
    SkillSyncCompleted {
        provider_id: String,
        project_id: Option<String>,
        synced_count: usize,
    },
    SkillSyncFailed {
        provider_id: String,
        project_id: Option<String>,
        error: String,
    },
    SkillConflictDetected {
        skill_name: String,
        detail: String,
    },
}

#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<SameEvent>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    pub fn publish(&self, event: SameEvent) {
        let _ = self.tx.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<SameEvent> {
        self.tx.subscribe()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(4096)
    }
}
