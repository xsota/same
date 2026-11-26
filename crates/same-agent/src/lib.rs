pub mod adapters;
pub mod claude;
pub mod codex;
pub mod diff;
pub mod discovery;
pub mod manager;
pub mod permissions;
pub mod process;
pub mod quota;
pub mod stats;

pub use manager::{AgentManager, PromptRequest, ProviderCheckResult, ProviderSetup, SetupOutcome};
pub use same_acp::exec::{self, ExecPreference, ExecTarget};
