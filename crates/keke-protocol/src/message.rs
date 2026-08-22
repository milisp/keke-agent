//! Provider-neutral conversation shapes.
//!
//! Providers translate these into their own wire formats; nothing here may
//! encode a vendor's schema. When two vendors disagree on a field, the neutral
//! type carries the union and each provider drops what it cannot express.

use serde::Deserialize;
use serde::Serialize;

use crate::ToolCall;
use crate::ToolResult;

/// Who produced a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    /// A tool result being fed back to the model.
    Tool,
}

/// An inline image.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageBlock {
    /// Base64-encoded bytes, without a data-URI prefix.
    pub data: String,
    /// IANA media type, e.g. `image/png`.
    pub media_type: String,
}

/// One piece of a message's content.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Image(ImageBlock),
    /// Reasoning the model chose to expose. Kept distinct from `Text` so
    /// surfaces can render or hide it without heuristics.
    Thinking {
        text: String,
        /// An opaque token the provider issued alongside the reasoning.
        ///
        /// Anthropic's wire rejects a replayed thinking block that arrives
        /// without the signature it minted, so a conversation that dropped this
        /// would silently lose its reasoning context on the next turn. Nothing
        /// but the originating provider may interpret it — treat it as bytes to
        /// hand back unchanged.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    ToolCall(ToolCall),
    ToolResult(ToolResult),
}

impl ContentBlock {
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    /// Reasoning with no provider signature, which is every wire but Anthropic's.
    #[must_use]
    pub fn thinking(text: impl Into<String>) -> Self {
        Self::Thinking {
            text: text.into(),
            signature: None,
        }
    }
}

/// A full conversation message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    #[must_use]
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::text(text)],
        }
    }

    #[must_use]
    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: vec![ContentBlock::text(text)],
        }
    }

    /// Concatenate every [`ContentBlock::Text`] block, ignoring the rest.
    #[must_use]
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

/// Why the model stopped generating.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The model finished its answer.
    EndTurn,
    /// The model wants tools run before continuing.
    ToolUse,
    /// The output token budget was exhausted.
    MaxTokens,
    /// The client aborted the turn.
    Cancelled,
    /// The provider refused, with its stated reason.
    Refusal { message: String },
}

/// Token accounting for one model call.
///
/// Fields are additive so a turn's usage is the sum of its steps'.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Input tokens served from the provider's prompt cache. A subset of
    /// `input_tokens`, not an addition to it.
    pub cached_input_tokens: u64,
    pub reasoning_tokens: u64,
}

impl Usage {
    /// Accumulate `other` into `self`.
    pub fn add(&mut self, other: Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cached_input_tokens += other.cached_input_tokens;
        self.reasoning_tokens += other.reasoning_tokens;
    }

    #[must_use]
    pub fn total(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }
}
