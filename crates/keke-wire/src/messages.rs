//! The Anthropic `/messages` wire.
//!
//! Three things it does differently. The system prompt is a top-level field
//! rather than a message, so a neutral `Role::System` message has to be lifted
//! out of the array instead of translated in place. Tool results are `user`
//! content blocks that must sit adjacent to the assistant turn that asked for
//! them, so consecutive same-role messages are merged rather than sent as-is.
//! And the stream is block-structured: `content_block_start` opens an indexed
//! block, deltas fill it, `content_block_stop` closes it — a tool call's
//! arguments arrive as `input_json_delta` fragments under its own index.

use std::collections::BTreeMap;

use keke_protocol::ContentBlock;
use keke_protocol::Message;
use keke_protocol::ReasoningEffort;
use keke_protocol::Role;
use keke_protocol::StopReason;
use keke_protocol::ToolCallId;
use keke_protocol::ToolStatus;
use keke_protocol::Usage;
use keke_provider_api::ModelRequest;
use keke_provider_api::ProviderError;
use keke_provider_api::StreamChunk;
use keke_provider_api::ToolSpec;
use serde::Deserialize;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;

use crate::decode::Sink;
use crate::decode::WireDecoder;

/// The API version this crate's translation was written against.
///
/// A contract version, not a date to keep current: Anthropic has published no
/// GA value after this one, every current model is served under it, and a
/// later-looking date is refused rather than accepted as "newer". Anthropic
/// requires the header and pins breaking changes to it, so sending a fixed one
/// is what keeps a server-side revision from silently changing our parsing.
pub(crate) const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Last resort when a caller builds a `ModelRequest` by hand without a budget.
///
/// The engine always fills `max_output_tokens` from configuration, so this is
/// unreachable in a real turn — it exists because `max_tokens` is mandatory on
/// this wire and a request that omits it is rejected outright. It is not a knob:
/// the knob is `max-output-tokens` in `keke-config-types`.
const UNSET_MAX_OUTPUT_TOKENS: u32 = 4096;

/// The smallest thinking budget this wire accepts. Below it the request is
/// rejected outright, so a reply budget with no room for it means extended
/// thinking is left off rather than sent in a shape that cannot be served.
const MIN_THINKING_BUDGET: u32 = 1024;

/// What each rung of the neutral ladder buys on this wire.
///
/// This vendor takes a token budget where the OpenAI wires take a word, so the
/// translation has to name numbers. They live here, next to the format that
/// needs them, for the same reason [`UNSET_MAX_OUTPUT_TOKENS`] does: they are
/// how one wire spells a setting, not a setting of their own. The knob a
/// deployment turns is `reasoning-effort`.
fn thinking_budget(effort: ReasoningEffort) -> u32 {
    match effort {
        ReasoningEffort::Low => 4_096,
        ReasoningEffort::Medium => 8_192,
        ReasoningEffort::High => 16_384,
        ReasoningEffort::XHigh => 32_768,
        ReasoningEffort::Max => 65_536,
        // This wire has no rung above the top one, and `Ultra` is the ladder's
        // top: it buys the same budget here, since the extra it names on the
        // wires that take it is task delegation rather than more thinking.
        ReasoningEffort::Ultra => 65_536,
    }
}

/// Build a `/messages` body.
#[must_use]
pub fn messages_body(request: &ModelRequest, stream: bool) -> Value {
    let mut body = Map::new();
    body.insert("model".to_string(), json!(request.model));
    body.insert("stream".to_string(), json!(stream));
    let max_tokens = request.max_output_tokens.unwrap_or(UNSET_MAX_OUTPUT_TOKENS);
    body.insert("max_tokens".to_string(), json!(max_tokens));

    // The budget has to leave room for an answer, so it is capped below the
    // reply budget rather than sent as the ladder names it. A reply budget too
    // small to hold even the minimum leaves thinking off: a rejected request
    // buys no thinking at all.
    let thinking = request
        .reasoning_effort
        .map(|effort| thinking_budget(effort).min(max_tokens.saturating_sub(MIN_THINKING_BUDGET)))
        .filter(|budget| *budget >= MIN_THINKING_BUDGET);

    let (system, messages) = split_system(request);
    if !system.is_empty() {
        body.insert("system".to_string(), json!(system));
    }
    body.insert("messages".to_string(), json!(messages));

    if !request.tools.is_empty() {
        body.insert(
            "tools".to_string(),
            json!(request.tools.iter().map(wire_tool).collect::<Vec<_>>()),
        );
    }
    // This wire refuses a temperature alongside extended thinking, so the two
    // cannot both be honored. Effort wins: it was asked for explicitly, and
    // dropping it instead would leave a request that thinks less than the
    // session was configured to.
    match thinking {
        Some(budget) => {
            body.insert(
                "thinking".to_string(),
                json!({ "type": "enabled", "budget_tokens": budget }),
            );
        }
        None => {
            if let Some(temperature) = request.temperature {
                body.insert("temperature".to_string(), json!(temperature));
            }
        }
    }
    Value::Object(body)
}

/// Tools carry their schema under `input_schema`, with no `function` envelope.
fn wire_tool(tool: &ToolSpec) -> Value {
    json!({
        "name": tool.name,
        "description": tool.description,
        "input_schema": tool.input_schema,
    })
}

/// Lift every system instruction to the top level and translate the rest.
///
/// A `Role::System` message left in the array would be rejected: this wire has
/// exactly two roles. Concatenating instead of dropping is what keeps a
/// mid-conversation system note visible to the model.
fn split_system(request: &ModelRequest) -> (String, Vec<Value>) {
    let mut system = request.system.clone().unwrap_or_default();
    let mut turns: Vec<(&'static str, Vec<Value>)> = Vec::new();

    for message in &request.messages {
        if matches!(message.role, Role::System) {
            for block in &message.content {
                if let ContentBlock::Text { text } = block {
                    if !system.is_empty() {
                        system.push_str("\n\n");
                    }
                    system.push_str(text);
                }
            }
            continue;
        }
        let (role, blocks) = wire_message(message);
        if blocks.is_empty() {
            continue;
        }
        // Adjacent same-role turns are merged because a tool result must be the
        // `user` turn that directly answers the `assistant` turn holding the
        // `tool_use` block; two user turns in a row break that pairing.
        match turns.last_mut() {
            Some((last_role, last_blocks)) if *last_role == role => last_blocks.extend(blocks),
            _ => turns.push((role, blocks)),
        }
    }

    let messages = turns
        .into_iter()
        .map(|(role, content)| json!({ "role": role, "content": content }))
        .collect();
    (system, messages)
}

fn wire_message(message: &Message) -> (&'static str, Vec<Value>) {
    let assistant = matches!(message.role, Role::Assistant);
    let mut blocks = Vec::new();
    let mut results = Vec::new();
    for block in &message.content {
        match block {
            ContentBlock::Text { text } if !text.is_empty() => {
                blocks.push(json!({ "type": "text", "text": text }));
            }
            ContentBlock::Text { .. } => {}
            // Replayed only with the signature this wire minted; it rejects a
            // `thinking` block without one. Reasoning that arrived from another
            // vendor carries no signature and is dropped rather than forged.
            ContentBlock::Thinking {
                text,
                signature: Some(signature),
            } if !text.is_empty() => blocks.push(json!({
                "type": "thinking",
                "thinking": text,
                "signature": signature,
            })),
            ContentBlock::Thinking { .. } => {}
            ContentBlock::Image(image) => blocks.push(json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": image.media_type,
                    "data": image.data,
                },
            })),
            ContentBlock::ToolCall(call) => blocks.push(json!({
                "type": "tool_use",
                "id": call.id.as_str(),
                "name": call.name,
                // Arguments are a structured value here, not the JSON string
                // the OpenAI wires use.
                "input": crate::arguments_value(&call.arguments),
            })),
            ContentBlock::ToolResult(result) => results.push(json!({
                "type": "tool_result",
                "tool_use_id": result.id.as_str(),
                "content": crate::result_text(result),
                "is_error": matches!(result.status, ToolStatus::Error | ToolStatus::Denied),
            })),
        }
    }

    if results.is_empty() {
        return (if assistant { "assistant" } else { "user" }, blocks);
    }
    // A result answers the previous assistant turn, so it leads its own user
    // turn; anything else the neutral message carried follows it.
    results.extend(blocks);
    ("user", results)
}

/// One typed SSE frame.
///
/// The events share one envelope and differ only in which fields they populate,
/// so a single optional-everything struct beats a dozen single-use ones.
#[derive(Debug, Deserialize)]
struct MessageEvent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    index: Option<usize>,
    #[serde(default)]
    content_block: Option<BlockStart>,
    #[serde(default)]
    delta: Option<Delta>,
    #[serde(default)]
    message: Option<MessageStart>,
    #[serde(default)]
    usage: Option<WireUsage>,
    #[serde(default)]
    error: Option<WireError>,
}

#[derive(Debug, Deserialize)]
struct BlockStart {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

/// Serves both `content_block_delta` (text and argument fragments) and
/// `message_delta` (the stop reason), which is why the fields do not overlap.
#[derive(Debug, Deserialize)]
struct Delta {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    partial_json: Option<String>,
    /// Closes a `thinking` block. Opaque, and replayed unchanged on the next
    /// turn — this wire rejects a thinking block that comes back without it.
    #[serde(default)]
    signature: Option<String>,
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MessageStart {
    #[serde(default)]
    usage: Option<WireUsage>,
}

#[derive(Debug, Deserialize)]
struct WireError {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct WireUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
}

struct OpenCall {
    id: ToolCallId,
    arguments: String,
}

#[derive(Default)]
pub(crate) struct Decoder {
    calls: BTreeMap<usize, OpenCall>,
    usage: Usage,
    stop: Option<StopReason>,
}

impl WireDecoder for Decoder {
    fn on_frame(&mut self, data: &str, out: &mut Sink) {
        let data = data.trim();
        if data.is_empty() {
            return;
        }
        let event: MessageEvent = match serde_json::from_str(data) {
            Ok(event) => event,
            Err(error) => {
                out.fail(format!("undecodable messages frame: {error}"));
                return;
            }
        };
        match event.kind.as_str() {
            "message_start" => {
                if let Some(usage) = event.message.and_then(|message| message.usage) {
                    self.absorb(&usage);
                }
            }
            "content_block_start" => self.on_block_start(&event, out),
            "content_block_delta" => self.on_block_delta(&event, out),
            "content_block_stop" => self.close_call(event.index.unwrap_or_default(), out),
            "message_delta" => {
                if let Some(usage) = &event.usage {
                    self.absorb(usage);
                }
                if let Some(reason) = event.delta.and_then(|delta| delta.stop_reason) {
                    self.stop = Some(stop_reason(&reason));
                }
            }
            "message_stop" => {
                out.push(StreamChunk::Usage(self.usage));
                // A `message_stop` without a preceding `message_delta` still
                // ended the reply; the turn is complete either way.
                out.finish(self.stop.take().unwrap_or(StopReason::EndTurn));
            }
            "error" => self.on_error(event.error, out),
            // `ping`, and whatever else Anthropic adds under this version.
            other => tracing::trace!(event = other, "unhandled messages event"),
        }
    }

    fn on_end(&mut self, out: &mut Sink) {
        out.truncated("the messages stream ended without message_stop");
    }
}

impl Decoder {
    fn absorb(&mut self, usage: &WireUsage) {
        // Anthropic reports input counts once at the start and output counts
        // again at the end, so this is a merge rather than a replacement.
        if usage.input_tokens > 0 {
            self.usage.input_tokens = usage.input_tokens;
        }
        if usage.output_tokens > 0 {
            self.usage.output_tokens = usage.output_tokens;
        }
        if usage.cache_read_input_tokens > 0 {
            self.usage.cached_input_tokens = usage.cache_read_input_tokens;
        }
    }

    fn on_block_start(&mut self, event: &MessageEvent, out: &mut Sink) {
        let Some(block) = &event.content_block else {
            return;
        };
        if block.kind != "tool_use" {
            return;
        }
        let Some(id) = block.id.clone() else {
            out.fail("a tool_use block arrived without an id");
            return;
        };
        let id = ToolCallId::new(id);
        out.push(StreamChunk::ToolCallStart {
            id: id.clone(),
            name: block.name.clone().unwrap_or_default(),
        });
        self.calls.insert(
            event.index.unwrap_or_default(),
            OpenCall {
                id,
                arguments: String::new(),
            },
        );
    }

    fn on_block_delta(&mut self, event: &MessageEvent, out: &mut Sink) {
        let Some(delta) = &event.delta else { return };
        if let Some(text) = delta.thinking.as_ref().filter(|text| !text.is_empty()) {
            out.push(StreamChunk::ThinkingDelta(text.clone()));
        }
        if let Some(signature) = delta.signature.as_ref().filter(|value| !value.is_empty()) {
            out.push(StreamChunk::ThinkingSignature(signature.clone()));
        }
        if let Some(text) = delta.text.as_ref().filter(|text| !text.is_empty()) {
            out.push(StreamChunk::TextDelta(text.clone()));
        }
        let Some(fragment) = delta
            .partial_json
            .as_ref()
            .filter(|fragment| !fragment.is_empty())
        else {
            return;
        };
        let index = event.index.unwrap_or_default();
        let Some(call) = self.calls.get_mut(&index) else {
            out.fail(format!(
                "an input_json_delta arrived for block {index} before its tool_use block"
            ));
            return;
        };
        call.arguments.push_str(fragment);
        out.push(StreamChunk::ToolCallArgsDelta {
            id: call.id.clone(),
            delta: fragment.clone(),
        });
    }

    /// `content_block_stop` fires for text blocks too, so a missing entry is
    /// ordinary rather than an error.
    fn close_call(&mut self, index: usize, out: &mut Sink) {
        let Some(call) = self.calls.remove(&index) else {
            return;
        };
        if let Err(error) = crate::check_arguments(&call.arguments) {
            out.fail(format!(
                "unparseable arguments for tool call {}: {error}",
                call.id
            ));
            return;
        }
        out.push(StreamChunk::ToolCallEnd { id: call.id });
    }

    /// A mid-stream `error` event. The status line was 200, so the taxonomy has
    /// to come from the payload's own type rather than from HTTP.
    fn on_error(&mut self, error: Option<WireError>, out: &mut Sink) {
        let error = error.unwrap_or(WireError {
            kind: None,
            message: None,
        });
        let detail = error
            .message
            .unwrap_or_else(|| "the provider failed the message".to_string());
        out.abort(match error.kind.as_deref() {
            Some("authentication_error" | "permission_error") => {
                ProviderError::Unauthorized(detail)
            }
            Some("rate_limit_error") => ProviderError::RateLimited {
                retry_after_millis: None,
            },
            Some("invalid_request_error") => ProviderError::InvalidRequest(detail),
            Some("not_found_error") => ProviderError::UnknownModel(detail),
            // `overloaded_error`, `api_error`, and anything new: the request was
            // accepted, so retrying it unchanged is the right move.
            _ => ProviderError::Transient(detail),
        });
    }
}

fn stop_reason(reason: &str) -> StopReason {
    match reason {
        "tool_use" | "pause_turn" => StopReason::ToolUse,
        "max_tokens" => StopReason::MaxTokens,
        "refusal" => StopReason::Refusal {
            message: "the provider refused to continue the reply".to_string(),
        },
        // "end_turn", "stop_sequence", plus anything added later: the reply did
        // end, and failing the turn over an unrecognized label would lose a
        // complete answer.
        other => {
            if !matches!(other, "end_turn" | "stop_sequence") {
                tracing::debug!(stop_reason = other, "unrecognized stop_reason");
            }
            StopReason::EndTurn
        }
    }
}
