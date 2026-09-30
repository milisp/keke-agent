//! Per-agent persistent memory.
//!
//! One markdown file per entry in an operator-chosen directory, two tools for
//! the model (`memory_read`, `memory_write`), and a summary of what exists in
//! the system prompt. The directory is configuration, never a default: one
//! installation running several named agents gives each its own.
//!
//! The summary is computed once at [`install`] and frozen for the session, so
//! the system prompt is stable and prompt-cache friendly; writes made during the
//! session are visible through `memory_read` and in the next session's summary.
//! It reaches the model as a `ContextFragment`, which the engine logs as
//! `SessionEvent::ContextFragment` (`AGENTS.md` invariant 6), so nothing here
//! needs an event of its own.

mod store;
mod summary;
mod tools;

pub use tools::MemoryRead;
pub use tools::MemoryWrite;

use std::sync::Arc;

use keke_config_types::MemoryConfig;
use keke_paths::AbsPath;
use keke_plugin_api::ContextContributor;
use keke_plugin_api::ContextFragment;
use keke_plugin_api::ExtFuture;
use keke_plugin_api::ExtensionContext;
use keke_plugin_api::ExtensionRegistryBuilder;
use keke_plugin_api::ToolContributor;
use keke_tool::ArcTool;

use store::Store;

/// After the environment block (50) and before tool guidance (100): it is about
/// the agent's own situation, not about how to call a tool.
const ORDER_MEMORY: i32 = 60;

struct MemoryExtension {
    store: Arc<Store>,
    /// Frozen at install. `None` when the summary budget is zero.
    summary: Option<String>,
}

impl ToolContributor for MemoryExtension {
    fn tools(&self, _ctx: &ExtensionContext) -> Vec<ArcTool> {
        vec![
            Arc::new(MemoryRead {
                store: Arc::clone(&self.store),
            }),
            Arc::new(MemoryWrite {
                store: Arc::clone(&self.store),
            }),
        ]
    }
}

impl ContextContributor for MemoryExtension {
    fn contribute_turn_context<'a>(
        &'a self,
        _ctx: &'a ExtensionContext,
    ) -> ExtFuture<'a, Vec<ContextFragment>> {
        Box::pin(async move {
            self.summary
                .iter()
                .map(|text| ContextFragment::new("memory", ORDER_MEMORY, text.clone()))
                .collect()
        })
    }
}

/// Register the memory tools and summary over `dir`.
///
/// `limits.dir` is ignored — `dir` is the directory — so a caller that already
/// resolved the configured directory against a flag passes the winner. The
/// directory need not exist yet; it is created on the first write.
///
/// The tools stay available under a `read_only` sandbox: that mode governs the
/// workspace, and memory is the agent's own state.
pub fn install(registry: &mut ExtensionRegistryBuilder, dir: AbsPath, limits: &MemoryConfig) {
    let store = Arc::new(Store::new(dir, limits.entry_max_bytes));
    // An unreadable directory means no listing, not no session: the tools
    // report the same fault to the model when it asks.
    let entries = store.list().unwrap_or_default();
    let summary = summary::render(
        store.dir().as_str(),
        &entries,
        limits.summary_max_bytes as usize,
    );
    let extension = Arc::new(MemoryExtension { store, summary });
    registry.tool_contributor(Arc::clone(&extension) as Arc<dyn ToolContributor>);
    registry.context_contributor(extension as Arc<dyn ContextContributor>);
}

#[cfg(test)]
mod tests;
