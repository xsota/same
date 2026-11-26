pub mod adapters;
pub mod claude;
pub mod codex;
pub mod diff;
pub mod discovery;
pub mod manager;
pub mod permissions;
pub mod process;
pub mod quota;
pub mod skills;
pub mod stats;

pub use manager::{
    AgentManager, InteractiveLoginEvent, InteractiveLoginHandle, PromptRequest, PromptSkillSync,
    ProviderCheckResult, ProviderSetup, SetupOutcome,
};
pub use same_acp::exec::{self, ExecPreference, ExecTarget};
pub use skills::{
    discover_external_skills, generate_skill_draft, provider_skill_method, GeneratedSkillDraft,
    SkillDeliveryPlan, SkillSynchronizer,
};
