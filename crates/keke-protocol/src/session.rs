//! The append-only session log.
//!
//! Every durable fact about a session is a [`SessionEvent`] appended to the
//! rollout log. The rule the engine enforces is *model-visible implies logged*:
//! if something reached a model request and is not reconstructable from these
//! events, replay diverges from the live run. Adding a new kind of model-visible
//! input therefore means adding a variant here first.

use serde::Deserialize;
use serde::Serialize;

use crate::Message;
use crate::StopReason;
use crate::ToolCall;
use crate::ToolResult;
use crate::TurnId;
use crate::Usage;

/// A durable fact about a session.
///
/// Variants are additive: readers must tolerate unknown ones, which is why
/// deserialization of a log is best-effort per line rather than all-or-nothing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionEvent {
    /// Opens the log. Records what the session was configured with.
    SessionStart {
        cwd: String,
        provider: String,
        model: String,
    },
    /// A turn began, with the user input that started it.
    TurnStart {
        turn: TurnId,
        input: Message,
    },
    /// Context assembled and handed to the model for one step. Logged in full
    /// because it is the model-visible input.
    ModelRequest {
        turn: TurnId,
        messages: Vec<Message>,
        /// Tool names advertised for this step, in the order presented.
        tools: Vec<String>,
    },
    /// The model's reply for one step.
    ModelResponse {
        turn: TurnId,
        message: Message,
        stop_reason: StopReason,
        usage: Usage,
    },
    ToolCallStart {
        turn: TurnId,
        call: ToolCall,
    },
    ToolCallEnd {
        turn: TurnId,
        result: ToolResult,
    },
    /// History was compacted; `summary` replaced the elided messages.
    Compacted {
        turn: TurnId,
        summary: Message,
        removed_messages: usize,
    },
    TurnEnd {
        turn: TurnId,
        stop_reason: StopReason,
        usage: Usage,
    },
    /// A turn failed. The session stays usable; the next turn resumes from the
    /// last consistent state.
    Error {
        turn: Option<TurnId>,
        message: String,
    },
}

/// A log line: one event plus the metadata every line carries.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionEventEnvelope {
    /// RFC 3339 timestamp of when the event was appended.
    pub at: String,
    #[serde(flatten)]
    pub event: SessionEvent,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ContentBlock;
    use crate::Role;

    #[test]
    fn envelope_flattens_event_fields() {
        let envelope = SessionEventEnvelope {
            at: "2026-08-21T00:00:00Z".to_string(),
            event: SessionEvent::TurnStart {
                turn: TurnId::new(),
                input: Message::user("hi"),
            },
        };
        let json = serde_json::to_value(&envelope).expect("serialize");
        assert_eq!(json["kind"], "turn_start");
        assert_eq!(json["at"], "2026-08-21T00:00:00Z");

        let back: SessionEventEnvelope = serde_json::from_value(json).expect("round trip");
        assert_eq!(back, envelope);
    }

    #[test]
    fn message_text_ignores_non_text_blocks() {
        let message = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::thinking("hidden"),
                ContentBlock::text("shown"),
            ],
        };
        assert_eq!(message.text(), "shown");
    }
}
