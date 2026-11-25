pub mod adapters;
pub mod manager;

pub use manager::{AgentManager, PromptRequest, ProviderCheckResult, ProviderSetup, SetupOutcome};
pub use same_acp::exec::{self, ExecPreference, ExecTarget};
