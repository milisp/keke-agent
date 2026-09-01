//! The agent engine.
//!
//! `keke-core` owns the session lifecycle, the turn loop, tool dispatch, and the
//! rollout log. It depends only on the contract tier, and it contains nothing
//! vendor-specific — no `keke-provider-*` or `keke-auth-*` dependency, no vendor
//! name in a match arm. `scripts/check-layering.py` enforces that in CI, because
//! the reference implementation that stated the same rule in prose alone did not
//! keep it.
//!
//! A session holds a provider, a tool set, and an extension registry, all
//! behind their contract traits. Swapping a vendor means constructing the
//! session with a different `Arc<dyn ModelProvider>`.

mod approval;
mod compact;
mod dispatch;
mod effort;
mod meta;
mod mode;
mod model;
mod prompt;
mod prompt_history;
mod resume;
mod rollout;
mod session;
mod tier;
mod turn;

pub use approval::ApprovalMemory;
pub use approval::ApprovalSwitch;
pub use approval::approval_reason;
pub use compact::estimate_history;
pub use compact::estimate_tokens;
pub use compact::should_compact;
pub use dispatch::Dispatch;
pub use dispatch::Dispatched;
pub use dispatch::ToolSet;
pub use dispatch::dispatch;
pub use effort::EffortSwitch;
pub use meta::SessionMeta;
pub use mode::SessionModeSwitch;
pub use model::ModelSwitch;
pub use prompt::ORDER_ENVIRONMENT;
pub use prompt::ORDER_IDENTITY;
pub use prompt::ORDER_PROJECT;
pub use prompt::assemble_system_prompt;
pub use prompt_history::PromptHistory;
pub use prompt_history::PromptHistoryEntry;
pub use prompt_history::prompt_history_path;
pub use resume::ResumedSession;
pub use resume::SessionMatch;
pub use resume::SessionSummary;
pub use resume::abbreviation;
pub use resume::find_session;
pub use resume::history_from_log;
pub use resume::latest_session;
pub use resume::list_recent;
pub use resume::list_sessions;
pub use resume::load_session;
pub use resume::project_dir;
pub use resume::sessions_dir;
pub use resume::usage_from_log;
pub use rollout::RolloutError;
pub use rollout::RolloutRecorder;
pub use rollout::read_log;
pub use rollout::read_log_from;
pub use session::RewindPoint;
pub use session::Rewound;
pub use session::Session;
pub use session::SessionBuilder;
pub use session::SessionConfig;
pub use session::TurnUpdate;
pub use tier::ServiceTierSwitch;
pub use turn::TurnOutcome;

use keke_provider_api::ProviderError;

/// Why a session or turn failed.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error(transparent)]
    Rollout(#[from] RolloutError),
    #[error(transparent)]
    Workspace(#[from] keke_workspace::WorkspaceError),
    /// A working-tree snapshot could not be taken or put back. Only ever
    /// fatal to a rewind: a turn whose snapshot failed carries on and is
    /// simply not one the files can be wound back to.
    #[error(transparent)]
    Checkpoint(#[from] keke_checkpoint::CheckpointError),
    /// The model kept requesting tools without converging. A safety stop, not a
    /// budget — reaching it means something is wrong, so it is surfaced rather
    /// than silently truncating the turn.
    #[error("the turn made {steps} model calls without finishing")]
    StepLimit { steps: usize },
    #[error("a session needs {0}")]
    Incomplete(&'static str),
}
