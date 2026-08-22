//! The tool ABI.
//!
//! One trait, [`Tool`], is implemented by every tool regardless of where it came
//! from: built in, ported, discovered over MCP, or contributed by a plugin. The
//! engine never sees [`Tool`] directly — it works through [`ToolDyn`], which a
//! blanket impl derives from any [`Tool`].
//!
//! Two shape decisions matter and are load-bearing:
//!
//! * [`Tool::execute`] is an RPITIT returning `impl Future + Send` rather than an
//!   `#[async_trait]` method, so the hot path allocates no box. Object safety is
//!   provided separately by [`ToolDyn`], which boxes exactly once at the erasure
//!   boundary.
//! * Execution is a stream with a documented invariant: zero or more
//!   [`ToolEvent::Progress`] items followed by exactly one
//!   [`ToolEvent::Terminal`]. A stream that ends without a terminal is a
//!   protocol violation the dispatcher reports as an error rather than a silent
//!   empty result.

mod dynamic;
mod error;
mod stream;
mod types;

pub use dynamic::ArcTool;
pub use dynamic::ToolDyn;
pub use dynamic::TypedToolOutput;
pub use error::ToolError;
pub use stream::ToolEvent;
pub use stream::ToolStream;
pub use types::ListToolsContext;
pub use types::ToolCallContext;
pub use types::ToolCapabilities;
pub use types::ToolDescription;
pub use types::ToolId;
pub use types::ToolKind;

use std::future::Future;

use schemars::JsonSchema;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Structured output a tool produces, plus how the model should see it.
///
/// Rendering is separated from the value so a surface can present a rich view
/// while the model receives a compact one. Both must be pure functions of the
/// value: they are called live *and* during log replay, and must agree.
pub trait ToolOutput {
    /// What the model sees. Keep it terse; this consumes context budget.
    fn render(&self) -> Vec<keke_protocol::ContentBlock>;
}

/// The unified tool trait.
///
/// Implement either [`Tool::run`] (a plain call) or [`Tool::execute`] (a
/// streaming call). The runtime only ever invokes `execute`; the default
/// `execute` wraps `run`. A tool that overrides neither fails loudly on first
/// call rather than returning an empty success.
pub trait Tool: Send + Sync + 'static {
    type Args: DeserializeOwned + JsonSchema + Send + 'static;
    type Output: Serialize + ToolOutput + Send + 'static;

    /// The tool's identity. Stable across versions; the model sees it.
    fn id(&self) -> ToolId;

    /// The model-facing description, which may depend on which other tools are
    /// present in this turn — a `grep` tool can point at `read_file` only when
    /// `read_file` is actually available.
    fn description(&self, ctx: &ListToolsContext) -> ToolDescription;

    /// Static facts the engine needs before running the tool: whether it is
    /// read-only, whether it may run concurrently with siblings, its timeout.
    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities::default()
    }

    /// Whether to advertise this tool for the given turn.
    fn should_list(&self, _ctx: &ListToolsContext) -> bool {
        true
    }

    /// Run the tool, emitting progress before the single terminal item.
    fn execute(
        &self,
        ctx: ToolCallContext,
        args: Self::Args,
    ) -> impl Future<Output = ToolStream<Self::Output>> + Send {
        async move { ToolStream::terminal_only(self.run(ctx, args).await) }
    }

    /// Run the tool to completion without progress reporting.
    fn run(
        &self,
        _ctx: ToolCallContext,
        _args: Self::Args,
    ) -> impl Future<Output = Result<Self::Output, ToolError>> + Send {
        async move {
            Err(ToolError::not_implemented(
                "tool implements neither `run` nor `execute`",
            ))
        }
    }
}
