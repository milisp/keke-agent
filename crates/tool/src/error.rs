//! The tool error taxonomy.
//!
//! The distinction that matters is *who should act*. `InvalidArgs` and
//! `Execution` are the model's problem — it sees them and may retry. `Denied`,
//! `Cancelled`, and `Timeout` are the harness's decisions and are reported as
//! such, so the model is never told "the tool failed" when policy refused it.

use keke_protocol::ToolStatus;

/// Why a tool call did not produce a value.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// Arguments failed to decode against the tool's schema.
    #[error("invalid arguments for `{tool}`: {message}")]
    InvalidArgs { tool: String, message: String },

    /// The tool ran and failed. `code` is a stable, machine-readable slug.
    #[error("{code}: {message}")]
    Execution { code: String, message: String },

    /// Policy refused the call. Raised by a guard before the body runs, and
    /// by a tool that rejects its own arguments on policy grounds — a path
    /// escaping the workspace, say. Either way the model should not retry
    /// the same call, which is what separates this from `Execution`.
    #[error("denied: {reason}")]
    Denied { reason: String },

    /// The turn was aborted.
    #[error("cancelled")]
    Cancelled,

    /// The call exceeded its budget.
    #[error("timed out after {millis}ms")]
    Timeout { millis: u64 },

    /// The tool declared a capability it does not implement.
    #[error("not implemented: {0}")]
    NotImplemented(String),
}

impl ToolError {
    /// An execution failure with a stable code.
    pub fn custom(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Execution {
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn not_implemented(message: impl Into<String>) -> Self {
        Self::NotImplemented(message.into())
    }

    pub fn denied(reason: impl Into<String>) -> Self {
        Self::Denied {
            reason: reason.into(),
        }
    }

    /// How this failure appears in the transcript.
    ///
    /// A denial is not an error the model should try to work around, so it maps
    /// to [`ToolStatus::Denied`] rather than [`ToolStatus::Error`].
    #[must_use]
    pub fn status(&self) -> ToolStatus {
        match self {
            Self::Denied { .. } => ToolStatus::Denied,
            Self::Cancelled => ToolStatus::Cancelled,
            Self::Timeout { .. } => ToolStatus::Cancelled,
            Self::InvalidArgs { .. } | Self::Execution { .. } | Self::NotImplemented(_) => {
                ToolStatus::Error
            }
        }
    }
}
