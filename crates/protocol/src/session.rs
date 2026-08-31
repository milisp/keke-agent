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
use crate::SessionId;
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
        /// The session that spawned this one, when it is a subagent's.
        ///
        /// A child keeps its own log, which is indistinguishable from a
        /// person's session by everything else in it: same shape, same turns,
        /// same cwd. Without this, `keke resume --list` offers to continue a
        /// conversation nobody had. A log written before this field existed, or
        /// one belonging to a session a person started, simply has none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<SessionId>,
    },
    /// A turn began, with the user input that started it.
    TurnStart {
        turn: TurnId,
        input: Message,
        /// The approval policy in force for this turn, so a resumed session
        /// picks up whatever a person last switched it to rather than
        /// whatever the config file says. Not model-visible — it never
        /// reaches a request — so it is carried here as a plain wire string
        /// rather than a typed field: `keke-config-types::ApprovalPolicy`
        /// sits above this crate and cannot be named from it.
        /// A log written before this field existed simply has none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        approval_policy: Option<String>,
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
        /// The model the request named. Beside `SessionStart`'s because the
        /// model can change while the session runs, and a log that only said
        /// what the session opened with could not say which model answered.
        /// A log written before this field existed simply has none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
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
    /// A tool the vendor executed for itself, inside the model call, rather
    /// than one the engine dispatched.
    ///
    /// Distinct from [`Self::ToolCallStart`]/[`Self::ToolCallEnd`]: those name
    /// a call the engine looks up in its own tool registry and runs, and a
    /// hosted tool has no entry there to find — OpenAI's and xAI's `web_search`
    /// run at the vendor, where neither the approval seam nor a `ToolGuard`
    /// can see them. Without a line of its own, a search the model acted on
    /// would be model-visible and unlogged, which invariant 6 forbids.
    HostedToolCall {
        turn: TurnId,
        /// The hosted tool's name, as the vendor named it (e.g. `web_search`).
        name: String,
        /// The query the vendor's tool ran, when the wire reports one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        query: Option<String>,
    },
    /// Model-visible text an extension put in front of the model.
    ///
    /// A `ContextContributor`'s fragment reaches the request inside the *system*
    /// prompt, which `ModelRequest` does not carry — it records `messages` and
    /// `tools`. Without a line of its own, a fragment that changed how the model
    /// behaved would be nowhere in the log, and *model-visible implies logged*
    /// would hold only for the parts of the request that happen to be messages.
    ContextFragment {
        /// Absent when the fragment was assembled outside a turn.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn: Option<TurnId>,
        /// The fragment's stable name, as its contributor gave it.
        name: String,
        text: String,
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
    /// A subagent was started for this turn.
    ///
    /// The child keeps its own log; this is the parent's record that it exists.
    /// Without it the child's tokens and tool calls would be spent under a
    /// session id nothing in the parent's log ever mentions, and a transcript
    /// that cannot name its own children cannot account for what a turn did.
    SubagentStart {
        turn: TurnId,
        /// The handle the model sees and passes back to collect the result.
        agent: String,
        /// The instruction the child was given. Model-visible input to the
        /// child, so it is logged in full rather than summarized.
        task: String,
    },
    /// A subagent finished, was cancelled, or timed out.
    SubagentEnd {
        turn: TurnId,
        agent: String,
        /// The child's own session, so its log can be found from here. Absent
        /// when the child never got far enough to open one — a spawn refused
        /// at the pool, or a parent cancelled before it started.
        session: Option<SessionId>,
        /// `completed`, `failed`, `timed_out`, or `cancelled`.
        status: String,
        /// What the child reported back, which is what reaches the parent's
        /// model. Empty when it produced nothing.
        summary: String,
        /// Charged to the parent's account of the turn: a child's tokens are
        /// spent because the parent asked for them.
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
                approval_policy: None,
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
    fn what_the_model_was_asked_survives_a_round_trip_through_the_log() {
        let envelope = SessionEventEnvelope {
            at: "2026-08-21T00:00:00Z".to_string(),
            event: SessionEvent::ModelRequest {
                turn: TurnId::new(),
                messages: vec![Message::user("hi")],
                tools: Vec::new(),
                reasoning_effort: Some(crate::ReasoningEffort::High),
                model: Some("grok-4.6".to_string()),
            },
        };
        let json = serde_json::to_value(&envelope).expect("serialize");
        assert_eq!(json["reasoning_effort"], "high");
        assert_eq!(json["model"], "grok-4.6");
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
