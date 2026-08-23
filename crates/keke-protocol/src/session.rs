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
use crate::ReasoningEffort;
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
        /// How hard the model was asked to think, when a level was set. It
        /// changes the reply, so it is part of the model-visible input; a log
        /// written before this field existed simply has none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_effort: Option<ReasoningEffort>,
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

    /// A field added later must not make older lines undecodable: the log is
    /// read line by line and a reader that rejected them would lose the session.
    #[test]
    fn a_model_request_logged_before_effort_existed_still_decodes() {
        let line = serde_json::json!({
            "at": "2026-08-21T00:00:00Z",
            "kind": "model_request",
            "turn": TurnId::new(),
            "messages": [],
            "tools": [],
        });
        let envelope: SessionEventEnvelope = serde_json::from_value(line).expect("decode");
        assert!(matches!(
            envelope.event,
            SessionEvent::ModelRequest {
                reasoning_effort: None,
                ..
            }
        ));
    }

    #[test]
    fn effort_survives_a_round_trip_through_the_log() {
        let envelope = SessionEventEnvelope {
            at: "2026-08-21T00:00:00Z".to_string(),
            event: SessionEvent::ModelRequest {
                turn: TurnId::new(),
                messages: vec![Message::user("hi")],
                tools: Vec::new(),
                reasoning_effort: Some(crate::ReasoningEffort::High),
            },
        };
        let json = serde_json::to_value(&envelope).expect("serialize");
        assert_eq!(json["reasoning_effort"], "high");
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
