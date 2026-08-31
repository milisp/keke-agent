//! The OpenAI `/responses` wire.
//!
//! Two things set it apart from chat completions. The request carries *input
//! items* rather than messages: a tool call and its result are top-level items
//! with their own types, not fields hanging off a message, so the neutral
//! conversation flattens rather than nests. And the stream is typed — every
//! frame names its own event — which means the decoder dispatches on a name
//! instead of diffing an index-keyed delta.
//!
//! Because `response.completed` carries the terminal status *and* the usage,
//! this decoder is the one format that does not have to hold a stop reason back.

use std::collections::BTreeMap;

use keke_protocol::ContentBlock;
use keke_protocol::ImageBlock;
use keke_protocol::Message;
use keke_protocol::Role;
use keke_protocol::StopReason;
use keke_protocol::ToolCallId;
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

/// Build a `/responses` body.
#[must_use]
pub fn responses_body(request: &ModelRequest, stream: bool, sampling_is_fixed: bool) -> Value {
    let mut body = Map::new();
    body.insert("model".to_string(), json!(request.model));
    body.insert("input".to_string(), json!(input_items(request)));
    body.insert("stream".to_string(), json!(stream));
    // Never stored server-side. The engine reconstructs every request from its
    // own `SessionEvent` log, so a copy held by the vendor would be a second
    // history that nothing keeps in step — and the ChatGPT backend refuses the
    // request outright without this: `{"detail": "Store must be set to false"}`.
    body.insert("store".to_string(), json!(false));
    // The system prompt is a first-class field here, not an item, so it is not
    // subject to the truncation this API applies to input items.
    if let Some(system) = &request.system {
        body.insert("instructions".to_string(), json!(system));
    }
    // Hosted tools travel in the same list as the harness's own: to this API a
    // tool the vendor executes and a tool it asks the caller to execute differ
    // only in their `type`. They go last so a vendor's tool can never displace
    // one the harness advertised under the same name.
    let tools: Vec<Value> = request
        .tools
        .iter()
        .map(wire_tool)
        .chain(request.hosted_tools.iter().cloned())
        .collect();
    if !tools.is_empty() {
        body.insert("tools".to_string(), Value::Array(tools));
    }
    // An endpoint that fixes its own sampling refuses to be told: naming either
    // of these is a 400 there, so the engine's budget is left unstated rather
    // than the turn failing over a control the backend was never going to honor.
    if !sampling_is_fixed {
        if let Some(max) = request.max_output_tokens {
            body.insert("max_output_tokens".to_string(), json!(max));
        }
        if let Some(temperature) = request.temperature {
            body.insert("temperature".to_string(), json!(temperature));
        }
    }
    // Nested under `reasoning` here rather than a top-level field, and sent as
    // written: a level this endpoint does not know is rejected by it, not
    // rounded down to one it does.
    if let Some(effort) = request.reasoning_effort {
        body.insert(
            "reasoning".to_string(),
            json!({ "effort": effort.as_str() }),
        );
    }
    crate::merge_vendor_params(&mut body, request);
    Value::Object(body)
}

/// Tools are flat here: no `function` envelope around name and parameters.
fn wire_tool(tool: &ToolSpec) -> Value {
    json!({
        "type": "function",
        "name": tool.name,
        "description": tool.description,
        "parameters": tool.input_schema,
    })
}

fn input_items(request: &ModelRequest) -> Vec<Value> {
    let mut out = Vec::new();
    for message in &request.messages {
        push_message(&mut out, message);
    }
    out
}

fn push_message(out: &mut Vec<Value>, message: &Message) {
    let start = out.len();
    let assistant = matches!(message.role, Role::Assistant);
    let role = match message.role {
        Role::Assistant => "assistant",
        Role::System => "system",
        // A tool message's own text has no item type of its own; it reads as
        // something the caller said, which is what `user` means here.
        Role::User | Role::Tool => "user",
    };
    let mut content = Vec::new();
    for block in &message.content {
        match block {
            ContentBlock::Text { text } | ContentBlock::Thinking { text, .. } => {
                content.push(json!({
                    "type": if assistant { "output_text" } else { "input_text" },
                    "text": text,
                }));
            }
            ContentBlock::Image(image) => content.push(json!({
                "type": "input_image",
                "image_url": data_uri(image),
            })),
            // Calls and results are items in their own right; emitting them
            // inline would leave the model with no `call_id` to match on.
            ContentBlock::ToolCall(call) => out.push(json!({
                "type": "function_call",
                "call_id": call.id.as_str(),
                "name": call.name,
                "arguments": crate::arguments_string(&call.arguments),
            })),
            ContentBlock::ToolResult(result) => out.push(json!({
                "type": "function_call_output",
                "call_id": result.id.as_str(),
                "output": crate::result_text(result),
            })),
        }
    }
    if !content.is_empty() {
        // The message item goes back where the message began: a model reads its
        // own narration before the calls that narration introduced, and the
        // calls were already appended as the blocks were walked.
        out.insert(
            start,
            json!({ "type": "message", "role": role, "content": content }),
        );
    }
}

fn data_uri(image: &ImageBlock) -> String {
    format!("data:{};base64,{}", image.media_type, image.data)
}

/// One typed SSE frame.
///
/// Every field is optional because the frames share one envelope and differ
/// only in which ones they populate; a per-event struct would be a dozen types
/// that each appear once.
#[derive(Debug, Deserialize)]
struct ResponseEvent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    delta: Option<String>,
    #[serde(default)]
    output_index: Option<usize>,
    #[serde(default)]
    item: Option<OutputItem>,
    #[serde(default)]
    response: Option<ResponseBody>,
    /// Populated by the bare `error` event, which has no `response` wrapper.
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OutputItem {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    call_id: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ResponseBody {
    #[serde(default)]
    usage: Option<WireUsage>,
    #[serde(default)]
    incomplete_details: Option<IncompleteDetails>,
    #[serde(default)]
    error: Option<WireError>,
}

#[derive(Debug, Deserialize)]
struct IncompleteDetails {
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireError {
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    input_tokens_details: Option<InputTokenDetails>,
    #[serde(default)]
    output_tokens_details: Option<OutputTokenDetails>,
}

#[derive(Debug, Deserialize)]
struct InputTokenDetails {
    #[serde(default)]
    cached_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct OutputTokenDetails {
    #[serde(default)]
    reasoning_tokens: u64,
}

impl From<WireUsage> for Usage {
    fn from(wire: WireUsage) -> Self {
        Self {
            input_tokens: wire.input_tokens,
            output_tokens: wire.output_tokens,
            cached_input_tokens: wire
                .input_tokens_details
                .map(|details| details.cached_tokens)
                .unwrap_or_default(),
            reasoning_tokens: wire
                .output_tokens_details
                .map(|details| details.reasoning_tokens)
                .unwrap_or_default(),
        }
    }
}

struct OpenCall {
    id: ToolCallId,
    arguments: String,
    ended: bool,
}

#[derive(Default)]
pub(crate) struct Decoder {
    calls: BTreeMap<usize, OpenCall>,
    saw_tool_call: bool,
    refused: Option<String>,
}

impl WireDecoder for Decoder {
    fn on_frame(&mut self, data: &str, out: &mut Sink) {
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            return;
        }
        let event: ResponseEvent = match serde_json::from_str(data) {
            Ok(event) => event,
            Err(error) => {
                out.fail(format!("undecodable responses frame: {error}"));
                return;
            }
        };
        match event.kind.as_str() {
            "response.output_text.delta" => {
                self.push_text(event.delta, out, StreamChunk::TextDelta)
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                self.push_text(event.delta, out, StreamChunk::ThinkingDelta);
            }
            "response.refusal.delta" => {
                self.refused
                    .get_or_insert_with(String::new)
                    .push_str(event.delta.as_deref().unwrap_or_default());
            }
            "response.output_item.added" => self.on_item_added(&event, out),
            "response.function_call_arguments.delta" => self.on_arguments_delta(&event, out),
            "response.function_call_arguments.done" | "response.output_item.done" => {
                self.close_call(event.output_index, out);
            }
            "response.completed" | "response.incomplete" => self.on_completed(&event, out),
            "response.failed" | "error" => self.on_failed(&event, out),
            // The API adds event types freely (`response.created`, deltas for
            // parts we do not surface); ignoring them is what keeps a new one
            // from failing an otherwise complete turn.
            other => tracing::trace!(event = other, "unhandled responses event"),
        }
    }

    fn on_end(&mut self, out: &mut Sink) {
        out.truncated("the responses stream ended without response.completed");
    }
}

impl Decoder {
    fn push_text(
        &mut self,
        delta: Option<String>,
        out: &mut Sink,
        chunk: fn(String) -> StreamChunk,
    ) {
        if let Some(text) = delta.filter(|text| !text.is_empty()) {
            out.push(chunk(text));
        }
    }

    fn on_item_added(&mut self, event: &ResponseEvent, out: &mut Sink) {
        let Some(item) = &event.item else { return };
        if item.kind != "function_call" {
            return;
        }
        // `call_id` is what a result must echo; `id` identifies the item and is
        // only a fallback for servers that omit the former.
        let Some(id) = item.call_id.clone().or_else(|| item.id.clone()) else {
            out.fail("a responses function_call item arrived without a call id");
            return;
        };
        let id = ToolCallId::new(id);
        self.saw_tool_call = true;
        out.push(StreamChunk::ToolCallStart {
            id: id.clone(),
            name: item.name.clone().unwrap_or_default(),
        });
        self.calls.insert(
            event.output_index.unwrap_or_default(),
            OpenCall {
                id,
                arguments: String::new(),
                ended: false,
            },
        );
    }

    fn on_arguments_delta(&mut self, event: &ResponseEvent, out: &mut Sink) {
        let index = event.output_index.unwrap_or_default();
        let Some(delta) = event.delta.clone().filter(|delta| !delta.is_empty()) else {
            return;
        };
        let Some(call) = self.calls.get_mut(&index) else {
            out.fail(format!(
                "responses sent argument deltas for output index {index} before its item"
            ));
            return;
        };
        call.arguments.push_str(&delta);
        out.push(StreamChunk::ToolCallArgsDelta {
            id: call.id.clone(),
            delta,
        });
    }

    /// Both `function_call_arguments.done` and `output_item.done` can close the
    /// same call, so ending is idempotent rather than a removal.
    fn close_call(&mut self, index: Option<usize>, out: &mut Sink) {
        let Some(call) = self.calls.get_mut(&index.unwrap_or_default()) else {
            return;
        };
        if call.ended {
            return;
        }
        if let Err(error) = crate::check_arguments(&call.arguments) {
            out.fail(format!(
                "unparseable arguments for tool call {}: {error}",
                call.id
            ));
            return;
        }
        call.ended = true;
        out.push(StreamChunk::ToolCallEnd {
            id: call.id.clone(),
        });
    }

    fn on_completed(&mut self, event: &ResponseEvent, out: &mut Sink) {
        let indexes: Vec<usize> = self.calls.keys().copied().collect();
        for index in indexes {
            self.close_call(Some(index), out);
            if out.is_complete() {
                return;
            }
        }
        let body = event.response.as_ref();
        if let Some(usage) = body.and_then(|body| body.usage.as_ref()) {
            out.push(StreamChunk::Usage(Usage {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cached_input_tokens: usage
                    .input_tokens_details
                    .as_ref()
                    .map(|details| details.cached_tokens)
                    .unwrap_or_default(),
                reasoning_tokens: usage
                    .output_tokens_details
                    .as_ref()
                    .map(|details| details.reasoning_tokens)
                    .unwrap_or_default(),
            }));
        }
        let truncated = body
            .and_then(|body| body.incomplete_details.as_ref())
            .and_then(|details| details.reason.as_deref())
            .is_some_and(|reason| reason == "max_output_tokens");
        out.finish(if let Some(message) = self.refused.take() {
            StopReason::Refusal { message }
        } else if truncated || event.kind == "response.incomplete" {
            StopReason::MaxTokens
        } else if self.saw_tool_call {
            StopReason::ToolUse
        } else {
            StopReason::EndTurn
        });
    }

    /// A response the server itself abandoned. Reported as transient because the
    /// request was accepted and could succeed unchanged — unlike a 4xx, which
    /// never reaches this decoder.
    fn on_failed(&mut self, event: &ResponseEvent, out: &mut Sink) {
        let detail = event
            .response
            .as_ref()
            .and_then(|body| body.error.as_ref())
            .and_then(|error| error.message.clone())
            .or_else(|| event.message.clone())
            .unwrap_or_else(|| "the provider failed the response".to_string());
        out.abort(ProviderError::Transient(detail));
    }
}
