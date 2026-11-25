pub mod adapters;
pub mod codex;
pub mod discovery;
pub mod manager;
pub mod permissions;

pub use manager::{AgentManager, PromptRequest, ProviderCheckResult, ProviderSetup, SetupOutcome};
pub use same_acp::exec::{self, ExecPreference, ExecTarget};
