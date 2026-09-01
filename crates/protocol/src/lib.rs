//! Vocabulary types for the whole workspace.
//!
//! This crate is deliberately inert: it holds identifiers, message shapes, and
//! the session event log entry, and it depends on nothing but `serde`. Every
//! other tier may depend on it; it depends on no other keke crate.
//!
//! The load-bearing invariant it exists to support is **model-visible implies
//! logged**: anything that reaches a model request must be reconstructable from
//! [`SessionEvent`]s, so a session can be replayed without re-running the model.

mod ids;
mod message;
mod reasoning;
mod session;
mod tool;

pub use ids::SessionId;
pub use ids::ThreadId;
pub use ids::ToolCallId;
pub use ids::TurnId;
pub use message::ContentBlock;
pub use message::ImageBlock;
pub use message::Message;
pub use message::Role;
pub use message::StopReason;
pub use message::Usage;
pub use reasoning::ReasoningEffort;
pub use session::RewindScope;
pub use session::SessionEvent;
pub use session::SessionEventEnvelope;
pub use tool::ToolCall;
pub use tool::ToolResult;
pub use tool::ToolStatus;
