//! The neutral streaming shape every provider produces.
//!
//! Vendors differ wildly in how they frame a stream — index-keyed deltas,
//! server-sent events with typed names, whole-message snapshots. Normalizing to
//! [`StreamChunk`] at the provider boundary is what keeps that variety out of
//! the engine's turn loop.

use futures::stream::BoxStream;
use keke_protocol::StopReason;
use keke_protocol::ToolCallId;
use keke_protocol::Usage;

use crate::ProviderError;

/// One normalized piece of a streamed model reply.
#[derive(Clone, Debug, PartialEq)]
pub enum StreamChunk {
    /// A fragment of visible assistant text.
    TextDelta(String),
    /// A fragment of exposed reasoning.
    ThinkingDelta(String),
    /// The opaque signature closing a reasoning block.
    ///
    /// Separate from the deltas because it arrives once, when the block ends,
    /// and because only the wires that mint one emit it at all.
    ThinkingSignature(String),
    /// A tool call has begun. Arguments arrive as subsequent deltas.
    ToolCallStart { id: ToolCallId, name: String },
    /// A fragment of the in-flight tool call's JSON arguments.
    ToolCallArgsDelta { id: ToolCallId, delta: String },
    /// The in-flight tool call is complete and its arguments are parseable.
    ToolCallEnd { id: ToolCallId },
    /// A tool the vendor ran for itself, inside this model call.
    ///
    /// Never dispatched locally — there is no [`ToolCallStart`]/[`ToolCallEnd`]
    /// pair to close, and no entry for it in the engine's tool registry to look
    /// up. It exists only so the engine can log that the vendor acted on the
    /// model's behalf; see `keke_protocol::SessionEvent::HostedToolCall`.
    ///
    /// [`ToolCallStart`]: Self::ToolCallStart
    /// [`ToolCallEnd`]: Self::ToolCallEnd
    HostedToolCall { name: String, query: Option<String> },
    /// Usage, which most vendors report only once at the end.
    Usage(Usage),
    /// The reply is finished. Always the last chunk of a successful stream.
    Done(StopReason),
}

/// A provider's reply stream.
///
/// The invariant mirrors the tool stream's: a successful stream ends with
/// exactly one [`StreamChunk::Done`]. A stream that ends without it is a
/// provider bug, and the engine reports it as [`ProviderError::Protocol`]
/// rather than treating the partial reply as complete.
pub type StreamEvent = BoxStream<'static, Result<StreamChunk, ProviderError>>;
