//! Extension points.
//!
//! Rather than one god `Plugin` trait, this crate defines a family of narrow
//! contributor traits. Every method has a default, so an extension implements
//! only the points it cares about, and adding a new point does not break
//! existing extensions.
//!
//! Composition is explicit and compile-time: each extension crate exposes a free
//! `install(&mut ExtensionRegistryBuilder, ..)` function, and the composition
//! root (`keke-cli`) calls them in order. There is no dynamic loading and no
//! dylib ABI to keep stable — runtime-installable plugins are *data* (skills,
//! MCP servers, hooks, commands), handled by `keke-plugin`.

mod approval;
mod context;
mod registry;

pub use approval::ApprovalDecision;
pub use approval::ApprovalRequest;
pub use context::ContextFragment;
pub use context::ExtensionContext;
pub use registry::ExtensionRegistry;
pub use registry::ExtensionRegistryBuilder;

use std::future::Future;
use std::pin::Pin;

use keke_protocol::StopReason;
use keke_protocol::ToolCall;
use keke_protocol::TurnId;
use keke_tool::ArcTool;
use keke_tool::ToolError;

/// A boxed future. Contributor traits are always held as `dyn`, so RPITIT is
/// not available here — unlike `keke_tool::Tool`, which is monomorphized.
pub type ExtFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Contributes tools.
pub trait ToolContributor: Send + Sync {
    /// Tools available for the whole session.
    fn tools(&self, _ctx: &ExtensionContext) -> Vec<ArcTool> {
        Vec::new()
    }
}

/// Contributes model-visible context.
///
/// Everything returned here reaches the model, so it must also be logged — the
/// engine records contributed fragments as session events for exactly that
/// reason.
pub trait ContextContributor: Send + Sync {
    /// Fragments injected once per turn.
    fn contribute_turn_context<'a>(
        &'a self,
        _ctx: &'a ExtensionContext,
    ) -> ExtFuture<'a, Vec<ContextFragment>> {
        Box::pin(async { Vec::new() })
    }
}

/// Observes turn boundaries.
pub trait TurnLifecycleContributor: Send + Sync {
    fn on_turn_start<'a>(&'a self, _ctx: &'a ExtensionContext, _turn: TurnId) -> ExtFuture<'a, ()> {
        Box::pin(async {})
    }

    fn on_turn_end<'a>(
        &'a self,
        _ctx: &'a ExtensionContext,
        _turn: TurnId,
        _reason: &'a StopReason,
    ) -> ExtFuture<'a, ()> {
        Box::pin(async {})
    }
}

/// Observes tool execution.
///
/// This is observation only. To *deny* a call, register a [`ToolGuard`] — the
/// separation is what makes denial monotonic.
pub trait ToolLifecycleContributor: Send + Sync {
    fn on_tool_start<'a>(
        &'a self,
        _ctx: &'a ExtensionContext,
        _call: &'a ToolCall,
    ) -> ExtFuture<'a, ()> {
        Box::pin(async {})
    }

    fn on_tool_finish<'a>(
        &'a self,
        _ctx: &'a ExtensionContext,
        _call: &'a ToolCall,
        _outcome: Result<(), &'a ToolError>,
    ) -> ExtFuture<'a, ()> {
        Box::pin(async {})
    }
}

/// Reviews approval requests.
///
/// The first contributor returning `Some` decides; later ones are not consulted.
/// Registration order is therefore priority order.
pub trait ApprovalReviewContributor: Send + Sync {
    fn review<'a>(
        &'a self,
        _ctx: &'a ExtensionContext,
        _request: &'a ApprovalRequest,
    ) -> ExtFuture<'a, Option<ApprovalDecision>>;
}

/// A monotonic denial check, run after every [`ToolLifecycleContributor`] and
/// before the tool body.
///
/// A guard can only deny — it has no "allow" result — so no ordering of guards
/// can turn a denial back into permission. This is the deliberate asymmetry that
/// keeps a permissive extension from being able to override a restrictive one.
pub type ToolGuard = Box<dyn Fn(&ToolCall) -> Option<String> + Send + Sync>;
