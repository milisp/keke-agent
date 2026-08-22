//! Neutral [`ModelRequest`] to xAI chat-completions JSON.
//!
//! xAI speaks the OpenAI `/chat/completions` schema, whose message shape is
//! flatter than the neutral one: a tool result is a message rather than a
//! content block, and a tool call hangs off the assistant message rather than
//! sitting inline with its text. The translation therefore splits one neutral
//! message into several wire messages, which is why this is a fold over blocks
//! rather than a per-message map.

use keke_protocol::ContentBlock;
use keke_protocol::ImageBlock;
use keke_protocol::Message;
use keke_protocol::Role;
use keke_protocol::ToolResult;
use keke_provider_api::ModelRequest;
use keke_provider_api::ToolSpec;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;

/// Build the request body. `stream` decides whether the caller gets SSE.
pub(crate) fn chat_completions_body(request: &ModelRequest, stream: bool) -> Value {
    let mut body = Map::new();
    body.insert("model".to_string(), json!(request.model));
    body.insert("messages".to_string(), json!(wire_messages(request)));
    body.insert("stream".to_string(), json!(stream));
    if stream {
        // Without this xAI omits the usage object entirely, and a turn with no
        // token accounting silently breaks budget tracking upstream.
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
            ContentBlock::Thinking { text: part } => reasoning.push_str(part),
            ContentBlock::ToolCall(call) => tool_calls.push(json!({
                "id": call.id.as_str(),
                "type": "function",
                "function": {
                    "name": call.name,
                    "arguments": arguments_string(&call.arguments),
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
            ContentBlock::Text { text } => parts.push(json!({ "type": "text", "text": text })),
            ContentBlock::Thinking { text } => parts.push(json!({ "type": "text", "text": text })),
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
    let mut text = String::new();
    for block in &result.content {
        match block {
            ContentBlock::Text { text: part } | ContentBlock::Thinking { text: part } => {
                text.push_str(part);
            }
            _ => {}
        }
    }
    json!({
        "role": "tool",
        "tool_call_id": result.id.as_str(),
        "content": text,
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

/// Tool call arguments travel as a JSON *string* on this wire, so a structured
/// value has to be re-encoded rather than embedded.
fn arguments_string(arguments: &Value) -> String {
    match arguments {
        Value::String(raw) => raw.clone(),
        other => other.to_string(),
    }
}
