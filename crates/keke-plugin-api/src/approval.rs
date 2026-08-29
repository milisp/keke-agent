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
    Allow {
        /// What the person said while allowing it, if they said anything.
        ///
        /// An approval is often not a bare yes — someone who approves a plan
        /// while asking for one thing to be different has answered *and*
        /// instructed, and the instruction is about the work the tool is
        /// entering, not the work after it. Carried with the answer so it
        /// reaches the model in the same turn; sent afterwards it would arrive
        /// once the thing it was meant to shape had already been done.
        note: Option<String>,
    },
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
