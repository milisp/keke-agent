//! Renders one [`Script`] into each of the three inference wire formats.
//!
//! The three vendors disagree about almost everything except the underlying
//! turn: where reasoning lives, whether tool arguments are a string or an
//! object, which frame carries usage, and what "the stream is over" looks like.
//! Keeping all three renderings side by side here is what makes it possible to
//! script an intent once and assert that every provider decodes it the same.

use serde_json::Value;
use serde_json::json;

use crate::reply::Part;
use crate::reply::Script;
use crate::reply::Stop;

const CHAT_ID: &str = "chatcmpl-mock";
const RESPONSE_ID: &str = "resp_mock";
const MESSAGE_ID: &str = "msg_mock";

use crate::sse::SseFrame;

fn joined(deltas: &[String]) -> String {
    deltas.concat()
}

// --- chat completions -------------------------------------------------------

fn chat_chunk(model: &str, delta: Value, finish: Value) -> SseFrame {
    SseFrame::data(
        json!({
            "id": CHAT_ID,
            "object": "chat.completion.chunk",
            "created": 0,
            "model": model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
        })
        .to_string(),
    )
}

fn chat_finish_reason(stop: Stop) -> &'static str {
    match stop {
        Stop::EndTurn => "stop",
        Stop::ToolUse => "tool_calls",
        Stop::MaxTokens => "length",
    }
}

pub(crate) fn chat_completions_stream(script: &Script, model: &str) -> Vec<SseFrame> {
    let mut frames = vec![chat_chunk(
        model,
        json!({ "role": "assistant" }),
        Value::Null,
    )];
    let mut tool_index = 0;

    for part in &script.parts {
        match part {
            Part::Text(deltas) => frames.extend(
                deltas
                    .iter()
                    .map(|d| chat_chunk(model, json!({ "content": d }), Value::Null)),
            ),
            Part::Thinking(deltas) => frames.extend(
                deltas
                    .iter()
                    .map(|d| chat_chunk(model, json!({ "reasoning_content": d }), Value::Null)),
            ),
            Part::ToolCall {
                id,
                name,
                frames: args,
            } => {
                frames.push(chat_chunk(
                    model,
                    json!({ "tool_calls": [{
                        "index": tool_index,
                        "id": id,
                        "type": "function",
                        "function": { "name": name, "arguments": "" },
                    }] }),
                    Value::Null,
                ));
                for arg in args {
                    frames.push(chat_chunk(
                        model,
                        json!({ "tool_calls": [{
                            "index": tool_index,
                            "function": { "arguments": arg },
                        }] }),
                        Value::Null,
                    ));
                }
                tool_index += 1;
            }
        }
    }

    if script.truncated {
        return frames;
    }

    if script.usage.is_some() {
        let usage = script.usage();
        frames.push(SseFrame::data(
            json!({
                "id": CHAT_ID,
                "object": "chat.completion.chunk",
                "created": 0,
                "model": model,
                "choices": [],
                "usage": {
                    "prompt_tokens": usage.input_tokens,
                    "completion_tokens": usage.output_tokens,
                    "total_tokens": usage.input_tokens + usage.output_tokens,
                },
            })
            .to_string(),
        ));
    }
    frames.push(chat_chunk(
        model,
        json!({}),
        json!(chat_finish_reason(script.stop())),
    ));
    frames.push(SseFrame::data("[DONE]"));
    frames
}

pub(crate) fn chat_completions_json(script: &Script, model: &str) -> Value {
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();

    for part in &script.parts {
        match part {
            Part::Text(deltas) => content.push_str(&joined(deltas)),
            Part::Thinking(deltas) => reasoning.push_str(&joined(deltas)),
            Part::ToolCall { id, name, frames } => tool_calls.push(json!({
                "id": id,
                "type": "function",
                "function": { "name": name, "arguments": joined(frames) },
            })),
        }
    }

    let mut message = json!({ "role": "assistant", "content": content });
    if !reasoning.is_empty() {
        message["reasoning_content"] = json!(reasoning);
    }
    if !tool_calls.is_empty() {
        message["tool_calls"] = json!(tool_calls);
    }
    let usage = script.usage();
    json!({
        "id": CHAT_ID,
        "object": "chat.completion",
        "created": 0,
        "model": model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": chat_finish_reason(script.stop()),
        }],
        "usage": {
            "prompt_tokens": usage.input_tokens,
            "completion_tokens": usage.output_tokens,
            "total_tokens": usage.input_tokens + usage.output_tokens,
        },
    })
}

// --- responses --------------------------------------------------------------

/// The completed `output` items, which the Responses API repeats verbatim in
/// `output_item.done` and again in the terminal response object.
fn response_items(script: &Script) -> Vec<Value> {
    script
        .parts
        .iter()
        .enumerate()
        .map(|(index, part)| match part {
            Part::Text(deltas) => json!({
                "id": format!("item_{index}"),
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": joined(deltas), "annotations": [] }],
            }),
            Part::Thinking(deltas) => json!({
                "id": format!("item_{index}"),
                "type": "reasoning",
                "summary": [{ "type": "summary_text", "text": joined(deltas) }],
            }),
            Part::ToolCall { id, name, frames } => json!({
                "id": format!("item_{index}"),
                "type": "function_call",
                "status": "completed",
                "call_id": id,
                "name": name,
                "arguments": joined(frames),
            }),
        })
        .collect()
}

fn response_object(script: &Script, model: &str, status: &str) -> Value {
    let usage = script.usage();
    let mut object = json!({
        "id": RESPONSE_ID,
        "object": "response",
        "created_at": 0,
        "status": status,
        "model": model,
        "output": response_items(script),
        "usage": {
            "input_tokens": usage.input_tokens,
            "output_tokens": usage.output_tokens,
            "total_tokens": usage.input_tokens + usage.output_tokens,
        },
    });
    if status == "incomplete" {
        object["incomplete_details"] = json!({ "reason": "max_output_tokens" });
    }
    object
}

pub(crate) fn responses_stream(script: &Script, model: &str) -> Vec<SseFrame> {
    let mut seq = 0;
    let mut emit = |name: &str, mut data: Value| {
        data["type"] = json!(name);
        data["sequence_number"] = json!(seq);
        seq += 1;
        SseFrame::named(name, data.to_string())
    };

    let mut frames = vec![emit(
        "response.created",
        json!({ "response": {
            "id": RESPONSE_ID, "object": "response", "created_at": 0,
            "status": "in_progress", "model": model, "output": [],
        } }),
    )];

    let done_items = response_items(script);
    for (index, part) in script.parts.iter().enumerate() {
        let item_id = format!("item_{index}");
        let done_item = done_items.get(index).cloned().unwrap_or(Value::Null);
        match part {
            Part::Text(deltas) => {
                frames.push(emit(
                    "response.output_item.added",
                    json!({ "output_index": index, "item": {
                        "id": item_id, "type": "message", "status": "in_progress",
                        "role": "assistant", "content": [],
                    } }),
                ));
                frames.push(emit(
                    "response.content_part.added",
                    json!({ "item_id": item_id, "output_index": index, "content_index": 0,
                        "part": { "type": "output_text", "text": "", "annotations": [] } }),
                ));
                for delta in deltas {
                    frames.push(emit(
                        "response.output_text.delta",
                        json!({ "item_id": item_id, "output_index": index,
                            "content_index": 0, "delta": delta }),
                    ));
                }
                frames.push(emit(
                    "response.output_text.done",
                    json!({ "item_id": item_id, "output_index": index,
                        "content_index": 0, "text": joined(deltas) }),
                ));
                frames.push(emit(
                    "response.content_part.done",
                    json!({ "item_id": item_id, "output_index": index, "content_index": 0,
                        "part": { "type": "output_text", "text": joined(deltas), "annotations": [] } }),
                ));
            }
            Part::Thinking(deltas) => {
                frames.push(emit(
                    "response.output_item.added",
                    json!({ "output_index": index, "item": {
                        "id": item_id, "type": "reasoning", "summary": [],
                    } }),
                ));
                frames.push(emit(
                    "response.reasoning_summary_part.added",
                    json!({ "item_id": item_id, "output_index": index, "summary_index": 0,
                        "part": { "type": "summary_text", "text": "" } }),
                ));
                for delta in deltas {
                    frames.push(emit(
                        "response.reasoning_summary_text.delta",
                        json!({ "item_id": item_id, "output_index": index,
                            "summary_index": 0, "delta": delta }),
                    ));
                }
                frames.push(emit(
                    "response.reasoning_summary_text.done",
                    json!({ "item_id": item_id, "output_index": index,
                        "summary_index": 0, "text": joined(deltas) }),
                ));
                frames.push(emit(
                    "response.reasoning_summary_part.done",
                    json!({ "item_id": item_id, "output_index": index, "summary_index": 0,
                        "part": { "type": "summary_text", "text": joined(deltas) } }),
                ));
            }
            Part::ToolCall {
                id,
                name,
                frames: args,
            } => {
                frames.push(emit(
                    "response.output_item.added",
                    json!({ "output_index": index, "item": {
                        "id": item_id, "type": "function_call", "status": "in_progress",
                        "call_id": id, "name": name, "arguments": "",
                    } }),
                ));
                for arg in args {
                    frames.push(emit(
                        "response.function_call_arguments.delta",
                        json!({ "item_id": item_id, "output_index": index, "delta": arg }),
                    ));
                }
                frames.push(emit(
                    "response.function_call_arguments.done",
                    json!({ "item_id": item_id, "output_index": index,
                        "arguments": joined(args) }),
                ));
            }
        }
        frames.push(emit(
            "response.output_item.done",
            json!({ "output_index": index, "item": done_item }),
        ));
    }

    if script.truncated {
        return frames;
    }

    let (name, status) = match script.stop() {
        Stop::MaxTokens => ("response.incomplete", "incomplete"),
        _ => ("response.completed", "completed"),
    };
    frames.push(emit(
        name,
        json!({ "response": response_object(script, model, status) }),
    ));
    frames
}

pub(crate) fn responses_json(script: &Script, model: &str) -> Value {
    let status = match script.stop() {
        Stop::MaxTokens => "incomplete",
        _ => "completed",
    };
    response_object(script, model, status)
}

// --- messages ---------------------------------------------------------------

fn messages_stop_reason(stop: Stop) -> &'static str {
    match stop {
        Stop::EndTurn => "end_turn",
        Stop::ToolUse => "tool_use",
        Stop::MaxTokens => "max_tokens",
    }
}

fn message_blocks(script: &Script) -> Vec<Value> {
    script
        .parts
        .iter()
        .map(|part| match part {
            Part::Text(deltas) => json!({ "type": "text", "text": joined(deltas) }),
            Part::Thinking(deltas) => json!({ "type": "thinking", "thinking": joined(deltas) }),
            Part::ToolCall { id, name, frames } => json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": serde_json::from_str::<Value>(&joined(frames)).unwrap_or(json!({})),
            }),
        })
        .collect()
}

pub(crate) fn messages_stream(script: &Script, model: &str) -> Vec<SseFrame> {
    let usage = script.usage();
    let event = |name: &str, mut data: Value| {
        data["type"] = json!(name);
        SseFrame::named(name, data.to_string())
    };

    let mut frames = vec![event(
        "message_start",
        json!({ "message": {
            "id": MESSAGE_ID, "type": "message", "role": "assistant", "model": model,
            "content": [], "stop_reason": Value::Null, "stop_sequence": Value::Null,
            "usage": { "input_tokens": usage.input_tokens, "output_tokens": 0 },
        } }),
    )];

    for (index, part) in script.parts.iter().enumerate() {
        let (block, deltas): (Value, Vec<Value>) = match part {
            Part::Text(deltas) => (
                json!({ "type": "text", "text": "" }),
                deltas
                    .iter()
                    .map(|d| json!({ "type": "text_delta", "text": d }))
                    .collect(),
            ),
            Part::Thinking(deltas) => (
                json!({ "type": "thinking", "thinking": "" }),
                deltas
                    .iter()
                    .map(|d| json!({ "type": "thinking_delta", "thinking": d }))
                    .collect(),
            ),
            Part::ToolCall {
                id,
                name,
                frames: args,
            } => (
                json!({ "type": "tool_use", "id": id, "name": name, "input": {} }),
                args.iter()
                    .map(|a| json!({ "type": "input_json_delta", "partial_json": a }))
                    .collect(),
            ),
        };
        frames.push(event(
            "content_block_start",
            json!({ "index": index, "content_block": block }),
        ));
        for delta in deltas {
            frames.push(event(
                "content_block_delta",
                json!({ "index": index, "delta": delta }),
            ));
        }
        frames.push(event("content_block_stop", json!({ "index": index })));
    }

    if script.truncated {
        return frames;
    }

    frames.push(event(
        "message_delta",
        json!({
            "delta": { "stop_reason": messages_stop_reason(script.stop()), "stop_sequence": Value::Null },
            "usage": { "output_tokens": usage.output_tokens },
        }),
    ));
    frames.push(event("message_stop", json!({})));
    frames
}

pub(crate) fn messages_json(script: &Script, model: &str) -> Value {
    let usage = script.usage();
    json!({
        "id": MESSAGE_ID,
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": message_blocks(script),
        "stop_reason": messages_stop_reason(script.stop()),
        "stop_sequence": Value::Null,
        "usage": { "input_tokens": usage.input_tokens, "output_tokens": usage.output_tokens },
    })
}
