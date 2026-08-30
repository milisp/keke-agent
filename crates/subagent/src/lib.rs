//! Subagents: isolated child sessions a model can start, run, and collect.
//!
//! A subagent is a whole session — same provider, same tools, same workspace,
//! its own rollout log — given one task and asked for one answer. What it is
//! *for* is context: the parent gets the conclusion without the thousand lines
//! of search output that produced it.
//!
//! Three shape decisions are load-bearing:
//!
//! * **A subagent cannot start a subagent.** Not by a configurable depth limit
//!   but by construction: the host remembers which sessions it created, and a
//!   child asking for its tool set is answered without these tools at all. A
//!   bound that only holds when someone configured it correctly is the failure
//!   mode the bound existed for.
//! * **One coordinator, not five handlers.** Both tools here are thin wrappers
//!   over [`SubagentHost`]; every lifecycle transition is recorded in one place,
//!   which is what makes *model-visible implies logged* checkable rather than
//!   hopeful (`AGENTS.md` invariant 6).
//! * **Nothing vendor-specific reaches this crate.** A child is built from a
//!   `SessionBuilder` the composition root already assembled, and that builder
//!   holds an `Arc<dyn ModelProvider>`. Which vendor answers is not a fact this
//!   crate can observe.
//!
//! # Known limitation
//!
//! A child inherits the approval policy the parent session was *configured*
//! with, not whatever a person has since switched it to with `/approval`. The
//! live switch is minted when the session is built, and the recipe is captured
//! before that. Raising the bar mid-session therefore does not tighten children
//! already spawnable from it; lowering it does not loosen them either.

mod host;
mod tools;

pub use host::AgentId;
pub use host::AgentProgress;
pub use host::AgentReport;
pub use host::AgentStatus;
pub use host::SubagentError;
pub use host::SubagentHost;
pub use tools::CollectAgent;
pub use tools::CollectAgentArgs;
pub use tools::CollectAgentOutput;
pub use tools::ReportedAgent;
pub use tools::SpawnAgent;
pub use tools::SpawnAgentArgs;
pub use tools::SpawnAgentOutput;

use std::sync::Arc;

use keke_config_types::SubagentLimits;
use keke_plugin_api::ExtensionContext;
use keke_plugin_api::ExtensionRegistryBuilder;
use keke_plugin_api::ToolContributor;
use keke_tool::ArcTool;

struct SubagentTools {
    host: Arc<SubagentHost>,
}

impl ToolContributor for SubagentTools {
    /// Built per turn from that turn's context, which is deliberate: the
    /// context is both how a tool learns whose session it is running in — the
    /// question that decides whether these tools exist at all — and how it
    /// records what it did.
    fn tools(&self, ctx: &ExtensionContext) -> Vec<ArcTool> {
        vec![
            Arc::new(SpawnAgent {
                host: Arc::clone(&self.host),
                ctx: ctx.clone(),
            }),
            Arc::new(CollectAgent {
                host: Arc::clone(&self.host),
                ctx: ctx.clone(),
            }),
        ]
    }
}

/// Register the subagent tools and return the host they share.
///
/// The caller must then hand the host the session recipe children are built
/// from — see [`SubagentHost::attach`]. Until it does, both tools decline to
/// list themselves: a `spawn_agent` the model can see but that cannot build a
/// session is worse than no `spawn_agent` at all.
///
/// The two steps are separate because the recipe contains the registry this
/// call is contributing to. Nothing can hold a finished `SessionBuilder` at the
/// moment the builder's own extensions are still being collected.
#[must_use]
pub fn install(
    registry: &mut ExtensionRegistryBuilder,
    limits: SubagentLimits,
) -> Arc<SubagentHost> {
    let host = Arc::new(SubagentHost::new(limits));
    registry.tool_contributor(Arc::new(SubagentTools {
        host: Arc::clone(&host),
    }));
    host
}
