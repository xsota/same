use crate::models::{
    ModelInfo, PermissionRequest, ProviderCapabilities, SessionStatus, SlashCommandInfo,
    ToolCallRecord,
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
        Self::new(1024)
    }
}
