//! Work a session leaves running: background shell commands, and the verbs
//! every kind of outstanding work shares.
//!
//! A background command is an ordinary tool call that returns before its child
//! does. Nothing here originates a turn — the model learns what a task said by
//! reading it with `task_output`, which is a normal tool result and therefore
//! already logged. That is deliberate: a source that could wake the agent on
//! its own would be model-visible input arriving outside a turn, and invariant
//! 6 in `AGENTS.md` would want a `SessionEvent` before it existed.

mod commands;
mod source;
mod tools;

pub use commands::BackgroundError;
pub use commands::BackgroundTasks;
pub use commands::KIND as COMMAND_KIND;
pub use source::TaskId;
pub use source::TaskOutput;
pub use source::TaskSnapshot;
pub use source::TaskSource;
pub use source::TaskSources;
pub use source::TaskState;
pub use tools::KillTask;
pub use tools::ListTasks;
pub use tools::TaskOutputTool;
pub use tools::WaitTasks;

use std::sync::Arc;

use keke_plugin_api::ExtensionContext;
use keke_plugin_api::ExtensionRegistryBuilder;
use keke_plugin_api::ToolContributor;
use keke_tool::ArcTool;

struct TaskTools {
    sources: TaskSources,
}

impl ToolContributor for TaskTools {
    /// The verbs are offered only when something can answer them. A
    /// `kill_task` in a composition with no task sources is a tool the model
    /// can call and never use successfully, which costs a turn to discover.
    fn tools(&self, _ctx: &ExtensionContext) -> Vec<ArcTool> {
        if self.sources.is_empty() {
            return Vec::new();
        }
        vec![
            Arc::new(ListTasks {
                sources: self.sources.clone(),
            }),
            Arc::new(TaskOutputTool {
                sources: self.sources.clone(),
            }),
            Arc::new(WaitTasks {
                sources: self.sources.clone(),
            }),
            Arc::new(KillTask {
                sources: self.sources.clone(),
            }),
        ]
    }
}

/// Register the shared task verbs over `sources`.
///
/// Composed once and frozen (`AGENTS.md` invariant 5): the source list is
/// built by the composition root and never added to afterwards, so no
/// contribution can outlive the composition that made it.
pub fn install(registry: &mut ExtensionRegistryBuilder, sources: Vec<Arc<dyn TaskSource>>) {
    registry.tool_contributor(Arc::new(TaskTools {
        sources: TaskSources::new(sources),
    }));
}

#[cfg(test)]
mod tests;
