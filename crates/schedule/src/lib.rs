//! Standing prompts, from the keyboard or from the model.
//!
//! The scheduler is pure and the surface owns the clock: nothing here starts a
//! turn. That is what keeps this crate below the surfaces — `/loop` and
//! `schedule_prompt` write the same records, and the interface that already has
//! an event loop is what fires them.
//!
//! A fired prompt reaches the model down the ordinary prompt path, so it is
//! logged as the user message it is (`AGENTS.md` invariant 6) without a new
//! `SessionEvent` for the timer that sent it.

mod handle;
mod scheduler;
mod tools;

pub use handle::Schedules;
pub use scheduler::KIND;
pub use scheduler::MAX_LIFETIME;
pub use scheduler::MAX_TASKS;
pub use scheduler::MIN_INTERVAL;
pub use scheduler::Origin;
pub use scheduler::Scheduler;
pub use scheduler::Task;
pub use scheduler::format_interval;
pub use scheduler::parse_id;
pub use scheduler::parse_interval;
pub use tools::SchedulePrompt;

use std::sync::Arc;

use keke_plugin_api::ExtensionContext;
use keke_plugin_api::ExtensionRegistryBuilder;
use keke_plugin_api::ToolContributor;
use keke_tool::ArcTool;

struct ScheduleTools {
    schedules: Schedules,
}

impl ToolContributor for ScheduleTools {
    fn tools(&self, _ctx: &ExtensionContext) -> Vec<ArcTool> {
        vec![Arc::new(SchedulePrompt {
            schedules: self.schedules.clone(),
        })]
    }
}

/// Register `schedule_prompt` over the session's scheduler.
///
/// The same `Schedules` must also go to the surface that fires loops and to
/// `keke_tasks::install` as a task source; a scheduler nothing reads is a set
/// of prompts that never arrive.
pub fn install(registry: &mut ExtensionRegistryBuilder, schedules: Schedules) {
    registry.tool_contributor(Arc::new(ScheduleTools { schedules }));
}

#[cfg(test)]
mod tests;
