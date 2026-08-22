//! Tool call and result shapes as they appear in the conversation.
//!
//! These are the *transcript* types. The executable side of a tool — its trait,
//! its streaming contract, its error taxonomy — lives in `keke-tool`, which
//! depends on this crate rather than the other way round.

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

use crate::ContentBlock;
use crate::ToolCallId;

/// A model's request to run one tool.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: ToolCallId,
    /// Fully-qualified tool name as advertised to the model.
    pub name: String,
    /// Raw arguments, validated against the tool's schema at dispatch time.
    pub arguments: Value,
}

/// How a tool call ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Ok,
    /// The tool ran and failed. The model sees the error and may retry.
    Error,
    /// Policy denied the call before the tool body ran.
    Denied,
    /// The turn was aborted, or the tool exceeded its timeout budget.
    Cancelled,
}

/// The outcome fed back to the model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    /// Echoes the originating [`ToolCall::id`] verbatim.
    pub id: ToolCallId,
    pub status: ToolStatus,
    /// What the model sees.
    pub content: Vec<ContentBlock>,
    /// The tool's structured output, retained for replay and for surfaces that
    /// render richer views than `content`. Never sent to the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
}

impl ToolResult {
    /// A successful text result.
    #[must_use]
    pub fn ok(id: ToolCallId, text: impl Into<String>) -> Self {
        Self {
            id,
            status: ToolStatus::Ok,
            content: vec![ContentBlock::text(text)],
            value: None,
        }
    }

    /// A failure the model is expected to read and react to.
    #[must_use]
    pub fn error(id: ToolCallId, text: impl Into<String>) -> Self {
        Self {
            id,
            status: ToolStatus::Error,
            content: vec![ContentBlock::text(text)],
            value: None,
        }
    }
}
