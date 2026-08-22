//! Supporting vocabulary for the [`Tool`](crate::Tool) trait.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use keke_paths::AbsPath;
use keke_protocol::ToolCallId;
use serde::Deserialize;
use serde::Serialize;

/// A tool's stable identifier as advertised to the model.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolId(String);

impl ToolId {
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ToolId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a tool does, coarsely.
///
/// The engine uses this for approval policy and for concurrency grouping, so it
/// must reflect real effects rather than a tool's self-image.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    Read,
    Search,
    Edit,
    Execute,
    Network,
    /// Affects only harness state, e.g. a todo list.
    Meta,
}

impl ToolKind {
    /// Whether this kind can change anything outside the harness.
    #[must_use]
    pub fn is_read_only(self) -> bool {
        matches!(self, Self::Read | Self::Search | Self::Meta)
    }
}

/// Static facts about a tool, read before it runs.
#[derive(Clone, Debug)]
pub struct ToolCapabilities {
    pub kind: ToolKind,
    /// Whether this call may run in parallel with other tool calls in the same
    /// step. Defaults to the kind's read-only-ness, which is the safe answer.
    pub concurrency_safe: bool,
    /// The tool's execution budget. Never sent to the model.
    ///
    /// The engine enforces this — a tool that overruns is cancelled and the
    /// call reported as [`ToolError::Timeout`]. It is surfaced to the running
    /// tool on [`ToolCallContext::timeout_millis`] so a tool with its own
    /// timeout argument can clamp to it instead of keeping a second number
    /// that can drift from this one.
    ///
    /// [`ToolError::Timeout`]: crate::ToolError::Timeout
    pub timeout_millis: Option<u64>,
}

impl Default for ToolCapabilities {
    fn default() -> Self {
        Self {
            kind: ToolKind::Meta,
            concurrency_safe: true,
            timeout_millis: None,
        }
    }
}

impl ToolCapabilities {
    #[must_use]
    pub fn of_kind(kind: ToolKind) -> Self {
        Self {
            kind,
            concurrency_safe: kind.is_read_only(),
            timeout_millis: None,
        }
    }
}

/// The model-facing description of a tool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolDescription {
    /// Prose shown to the model alongside the schema.
    pub text: String,
}

impl ToolDescription {
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

/// What a tool may consult when deciding whether and how to advertise itself.
///
/// Notably it includes the ids of every other tool being listed, so a tool's
/// prose can reference its siblings without hardcoding assumptions about which
/// of them exist.
#[derive(Clone, Debug, Default)]
pub struct ListToolsContext {
    /// Every tool id being advertised this turn, including this one.
    pub siblings: Vec<ToolId>,
    /// Free-form facts contributed by extensions, e.g. the active model family.
    pub attributes: BTreeMap<String, String>,
}

impl ListToolsContext {
    /// Whether a sibling tool is present this turn.
    #[must_use]
    pub fn has(&self, id: &str) -> bool {
        self.siblings.iter().any(|sibling| sibling.as_str() == id)
    }
}

/// Everything a tool needs while running.
///
/// The `cancel` handle is a plain flag rather than a `CancellationToken` so this
/// crate stays free of a runtime dependency; the engine supplies an
/// implementation backed by whatever it uses.
#[derive(Clone)]
pub struct ToolCallContext {
    /// The call this execution answers.
    pub call_id: ToolCallId,
    /// The workspace root. Tools must keep their effects inside it unless a
    /// capability explicitly says otherwise.
    pub workspace_root: AbsPath,
    /// The budget the engine is enforcing for this call, from
    /// [`ToolCapabilities::timeout_millis`]. A tool that takes its own timeout
    /// argument should clamp to this rather than exceed it — overrunning gets
    /// the call cancelled, which loses whatever partial output it had.
    pub timeout_millis: Option<u64>,
    /// Returns true once the turn has been aborted.
    pub cancelled: Arc<dyn Fn() -> bool + Send + Sync>,
}

impl ToolCallContext {
    /// Whether the turn has been aborted; long-running tools should poll this.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        (self.cancelled)()
    }
}

impl fmt::Debug for ToolCallContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolCallContext")
            .field("call_id", &self.call_id)
            .field("workspace_root", &self.workspace_root)
            .field("timeout_millis", &self.timeout_millis)
            .finish_non_exhaustive()
    }
}
