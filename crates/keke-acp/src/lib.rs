//! keke's Agent Client Protocol surface.
//!
//! ACP is both how an editor drives keke and how keke's own TUI does, which is
//! deliberate: a protocol only one side uses drifts, and dogfooding it is what
//! keeps editor support working. grok-build reached the same arrangement.
//!
//! Surfaces do not see ACP types. They see [`Conversation`] and [`Update`], so
//! the same TUI attaches to an in-process agent or a remote one without knowing
//! which.

mod conversation;

pub use conversation::Conversation;
pub use conversation::ConversationError;
pub use conversation::ConversationFuture;
pub use conversation::PermissionAnswer;
pub use conversation::PermissionId;
pub use conversation::ScriptedConversation;
pub use conversation::Update;
