//! The OpenAI `/chat/completions` wire.
//!
//! Its message shape is flatter than the neutral one: a tool result is a
//! message rather than a content block, and a tool call hangs off the assistant
//! message rather than sitting inline with its text. Translation therefore
//! splits one neutral message into several wire messages, which is why the
//! request side is a fold over blocks rather than a per-message map.
//!
//! The stream side is a state machine for two reasons. Tool calls arrive
//! index-keyed and fragmented — the id and name land on the first fragment and
//! the arguments accumulate over later ones — so a call is only complete once
//! the choice finishes. And `finish_reason` typically arrives one frame *before*
//! the usage object, while `Done` must be last; the stop reason is therefore
//! held back until the transport actually ends.

use std::collections::BTreeMap;

use keke_protocol::ContentBlock;
use keke_protocol::ImageBlock;
use keke_protocol::Message;
use keke_protocol::Role;
use keke_protocol::StopReason;
use keke_protocol::ToolCallId;
use keke_protocol::ToolResult;
use keke_protocol::Usage;
use keke_provider_api::ModelRequest;
use keke_provider_api::StreamChunk;
use keke_provider_api::ToolSpec;
use serde::Deserialize;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;

use crate::decode::Sink;
use crate::decode::WireDecoder;

/// Build a `/chat/completions` body. `stream` decides whether the caller gets
/// SSE back.
///
/// Public because it is also the honest way to test the translation: the shape
/// sent to a vendor is behavior, not an implementation detail.
#[must_use]
pub fn chat_completions_body(request: &ModelRequest, stream: bool) -> Value {
    let mut body = Map::new();
    body.insert("model".to_string(), json!(request.model));
    body.insert("messages".to_string(), json!(wire_messages(request)));
    body.insert("stream".to_string(), json!(stream));
    if stream {
        // Without this most vendors omit the usage object entirely, and a turn
        // with no token accounting silently breaks budget tracking upstream.
        body.insert(
            "stream_options".to_string(),
            json!({ "include_usage": true }),
        );
    }
    if !request.tools.is_empty() {
        body.insert(
            "tools".to_string(),
            json!(request.tools.iter().map(wire_tool).collect::<Vec<_>>()),
        );
    }
    if let Some(max) = request.max_output_tokens {
        body.insert("max_tokens".to_string(), json!(max));
    }
    if let Some(temperature) = request.temperature {
        body.insert("temperature".to_string(), json!(temperature));
    }
    // Sent as written, including a level this endpoint may not know: a vendor
    // rejecting `xhigh` is a visible failure, silently sending `high` instead
    // is not.
    if let Some(effort) = request.reasoning_effort {
        body.insert("reasoning_effort".to_string(), json!(effort.as_str()));
    }
    Value::Object(body)
}

fn wire_tool(tool: &ToolSpec) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": tool.name,
            "description": tool.description,
            "parameters": tool.input_schema,
        },
    })
}

fn wire_messages(request: &ModelRequest) -> Vec<Value> {
    let mut out = Vec::new();
    if let Some(system) = &request.system {
        out.push(json!({ "role": "system", "content": system }));
    }
    for message in &request.messages {
        push_message(&mut out, message);
    }
    out
}

fn push_message(out: &mut Vec<Value>, message: &Message) {
    match message.role {
        Role::Assistant => push_assistant(out, message),
        Role::System => {
            let text = joined_text(message);
            if !text.is_empty() {
                out.push(json!({ "role": "system", "content": text }));
            }
        }
        Role::User | Role::Tool => push_user_or_tool(out, message),
    }
}

/// Emits the assistant text and any tool calls as one message, then each tool
/// result the same message happened to carry as its own `role: "tool"` message.
fn push_assistant(out: &mut Vec<Value>, message: &Message) {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();
    let mut results = Vec::new();
    for block in &message.content {
        match block {
            ContentBlock::Text { text: part } => text.push_str(part),
            ContentBlock::Thinking { text: part, .. } => reasoning.push_str(part),
            ContentBlock::ToolCall(call) => tool_calls.push(json!({
                "id": call.id.as_str(),
                "type": "function",
                "function": {
                    "name": call.name,
                    "arguments": crate::arguments_string(&call.arguments),
                },
            })),
            ContentBlock::ToolResult(result) => results.push(result),
            // The wire schema has nowhere to put an image the assistant
            // produced, and inventing one would change what the model sees.
            ContentBlock::Image(_) => {}
        }
    }

    let mut wire = Map::new();
    wire.insert("role".to_string(), json!("assistant"));
    wire.insert("content".to_string(), json!(text));
    if !reasoning.is_empty() {
        wire.insert("reasoning_content".to_string(), json!(reasoning));
    }
    if !tool_calls.is_empty() {
        wire.insert("tool_calls".to_string(), json!(tool_calls));
    }
    out.push(Value::Object(wire));

    for result in results {
        out.push(wire_tool_result(result));
    }
}

/// User and tool messages differ only in what a non-result block becomes: both
/// may carry results, and a tool message's stray text would otherwise be
/// dropped, so it is preserved as user content rather than discarded.
fn push_user_or_tool(out: &mut Vec<Value>, message: &Message) {
    let mut parts = Vec::new();
    let mut results = Vec::new();
    for block in &message.content {
        match block {
            ContentBlock::Text { text } | ContentBlock::Thinking { text, .. } => {
                parts.push(json!({ "type": "text", "text": text }));
            }
            ContentBlock::Image(image) => parts.push(json!({
                "type": "image_url",
                "image_url": { "url": data_uri(image) },
            })),
            ContentBlock::ToolResult(result) => results.push(result),
            // A tool call is the model's output; it can only reach the wire on
            // an assistant message.
            ContentBlock::ToolCall(_) => {}
        }
    }

    // Results precede the text so the model reads them in the order they were
    // produced: the call it just made, answered, then anything the user added.
    for result in results {
        out.push(wire_tool_result(result));
    }
    if !parts.is_empty() {
        out.push(json!({ "role": "user", "content": parts }));
    }
}

fn wire_tool_result(result: &ToolResult) -> Value {
    json!({
        "role": "tool",
        "tool_call_id": result.id.as_str(),
        "content": crate::result_text(result),
    })
}

fn joined_text(message: &Message) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn data_uri(image: &ImageBlock) -> String {
    format!("data:{};base64,{}", image.media_type, image.data)
}

/// A frame of `data:` payload as this wire sends it.
#[derive(Debug, Deserialize)]
struct ChatChunk {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<WireUsage>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    #[serde(default)]
    delta: Delta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCallDelta>>,
}

#[derive(Debug, Deserialize)]
struct ToolCallDelta {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<FunctionDelta>,
}

#[derive(Debug, Deserialize)]
struct FunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokenDetails>,
    #[serde(default)]
    completion_tokens_details: Option<CompletionTokenDetails>,
}

#[derive(Debug, Deserialize)]
struct PromptTokenDetails {
    #[serde(default)]
    cached_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct CompletionTokenDetails {
    #[serde(default)]
    reasoning_tokens: u64,
}

impl From<WireUsage> for Usage {
    fn from(wire: WireUsage) -> Self {
        Self {
            input_tokens: wire.prompt_tokens,
            output_tokens: wire.completion_tokens,
            cached_input_tokens: wire
                .prompt_tokens_details
                .map(|details| details.cached_tokens)
                .unwrap_or_default(),
            reasoning_tokens: wire
                .completion_tokens_details
                .map(|details| details.reasoning_tokens)
                .unwrap_or_default(),
        }
    }
}

/// A tool call being assembled across frames.
struct OpenCall {
    id: ToolCallId,
    arguments: String,
}

#[derive(Default)]
pub(crate) struct Decoder {
    calls: BTreeMap<usize, OpenCall>,
    stop: Option<StopReason>,
    usage_seen: bool,
}

impl WireDecoder for Decoder {
    fn on_frame(&mut self, data: &str, out: &mut Sink) {
        let data = data.trim();
        if data.is_empty() {
            return;
        }
        if data == "[DONE]" {
            self.on_end(out);
            return;
        }
        let chunk: ChatChunk = match serde_json::from_str(data) {
            Ok(chunk) => chunk,
            Err(error) => {
                out.fail(format!("undecodable chat-completions frame: {error}"));
                return;
            }
        };
        if let Some(usage) = chunk.usage {
            // Some models repeat usage on the terminal frame; one `Usage` chunk
            // per call keeps the engine's accumulation additive.
            if !self.usage_seen {
                self.usage_seen = true;
                out.push(StreamChunk::Usage(usage.into()));
            }
        }
        for choice in chunk.choices {
            self.on_choice(choice, out);
        }
    }

    fn on_end(&mut self, out: &mut Sink) {
        match self.stop.take() {
            Some(stop) => out.finish(stop),
            None => out.truncated("the chat-completions stream ended without a finish_reason"),
        }
    }
}

impl Decoder {
    fn on_choice(&mut self, choice: Choice, out: &mut Sink) {
        if let Some(text) = choice.delta.reasoning_content.filter(|t| !t.is_empty()) {
            out.push(StreamChunk::ThinkingDelta(text));
        }
        if let Some(text) = choice.delta.content.filter(|t| !t.is_empty()) {
            out.push(StreamChunk::TextDelta(text));
        }
        for delta in choice.delta.tool_calls.unwrap_or_default() {
            self.on_tool_call(delta, out);
            if out.is_complete() {
                return;
            }
        }
        if let Some(reason) = choice.finish_reason {
            self.close_calls(out);
            self.stop = Some(stop_reason(&reason));
        }
    }

    fn on_tool_call(&mut self, delta: ToolCallDelta, out: &mut Sink) {
        let (name, arguments) = delta
            .function
            .map_or((None, None), |f| (f.name, f.arguments));
        if let Some(id) = delta.id.filter(|id| !id.is_empty()) {
            let id = ToolCallId::new(id);
            out.push(StreamChunk::ToolCallStart {
                id: id.clone(),
                name: name.unwrap_or_default(),
            });
            self.calls.insert(
                delta.index,
                OpenCall {
                    id,
                    arguments: String::new(),
                },
            );
        }
        let Some(call) = self.calls.get_mut(&delta.index) else {
            // Without an id there is nothing to echo back to the provider when
            // the result returns, so this cannot be recovered from.
            out.fail(format!(
                "a tool call fragment arrived for index {} before any call id",
                delta.index
            ));
            return;
        };
        if let Some(arguments) = arguments.filter(|a| !a.is_empty()) {
            call.arguments.push_str(&arguments);
            out.push(StreamChunk::ToolCallArgsDelta {
                id: call.id.clone(),
                delta: arguments,
            });
        }
    }

    /// `ToolCallEnd` promises parseable arguments, so the accumulated fragments
    /// are checked here rather than left for the dispatcher to trip over.
    fn close_calls(&mut self, out: &mut Sink) {
        for (_, call) in std::mem::take(&mut self.calls) {
            if let Err(error) = crate::check_arguments(&call.arguments) {
                out.fail(format!(
                    "unparseable arguments for tool call {}: {error}",
                    call.id
                ));
                return;
            }
            out.push(StreamChunk::ToolCallEnd { id: call.id });
        }
    }
}

fn stop_reason(reason: &str) -> StopReason {
    match reason {
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "length" | "max_tokens" => StopReason::MaxTokens,
        "content_filter" => StopReason::Refusal {
            message: "the provider stopped the reply for content filtering".to_string(),
        },
        "cancelled" | "aborted" => StopReason::Cancelled,
        // "stop", plus anything a vendor adds later: the reply did end, and
        // failing the turn over an unrecognized label would lose a complete
        // answer.
        other => {
            if other != "stop" {
                tracing::debug!(finish_reason = other, "unrecognized finish_reason");
            }
            StopReason::EndTurn
        }
    }
}
