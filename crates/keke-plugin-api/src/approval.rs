//! Approval requests and decisions.

use keke_protocol::ToolCall;

/// Something the harness wants permission for.
#[derive(Clone, Debug)]
pub struct ApprovalRequest {
    /// The tool call needing approval.
    pub call: ToolCall,
    /// Why approval is required, shown to whoever decides.
    pub reason: String,
}

/// The answer to an [`ApprovalRequest`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApprovalDecision {
    Allow,
    /// Allow this and every future call matching the same shape, for the
    /// remainder of the session.
    AllowAlways,
    Deny {
        reason: String,
    },
    /// Deny and abort the turn rather than letting the model try again.
    Abort {
        reason: String,
    },
}
