//! xAI SSE frames to neutral [`StreamChunk`]s.
//!
//! Two things force this to be a state machine rather than a per-frame map.
//! Tool calls arrive index-keyed and fragmented — the id and name land on the
//! first fragment and the arguments accumulate over later ones — so a call is
//! only complete once the choice finishes. And `finish_reason` typically
//! arrives one frame *before* the usage object, while `Done` must be the last
//! chunk of the stream; the stop reason is therefore held back until the
//! transport actually ends.

use std::collections::BTreeMap;
use std::collections::VecDeque;

use futures::StreamExt;
use futures::stream::BoxStream;
use keke_protocol::StopReason;
use keke_protocol::ToolCallId;
use keke_protocol::Usage;
use keke_provider_api::ProviderError;
use keke_provider_api::StreamChunk;
use serde::Deserialize;

/// A frame of `data:` payload as xAI sends it.
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

/// Payload of the `data:` field of one SSE frame.
pub(crate) type Frame = Result<String, ProviderError>;

struct Decoder {
    frames: BoxStream<'static, Frame>,
    calls: BTreeMap<usize, OpenCall>,
    stop: Option<StopReason>,
    usage_seen: bool,
    pending: VecDeque<Result<StreamChunk, ProviderError>>,
    finished: bool,
}

/// Decode a stream of SSE `data:` payloads into neutral chunks.
///
/// The returned stream ends with exactly one [`StreamChunk::Done`], or with a
/// [`ProviderError`]; it never ends silently on a truncated reply.
pub(crate) fn decode(
    frames: BoxStream<'static, Frame>,
) -> BoxStream<'static, Result<StreamChunk, ProviderError>> {
    let decoder = Decoder {
        frames,
        calls: BTreeMap::new(),
        stop: None,
        usage_seen: false,
        pending: VecDeque::new(),
        finished: false,
    };
    futures::stream::unfold(decoder, |mut decoder| async move {
        loop {
            if let Some(chunk) = decoder.pending.pop_front() {
                // An error is terminal: a decoder that kept going after one
                // would report a partial reply as if it were whole.
                decoder.finished |= chunk.is_err();
                return Some((chunk, decoder));
            }
            if decoder.finished {
                return None;
            }
            match decoder.frames.next().await {
                Some(Ok(data)) => decoder.on_frame(&data),
                Some(Err(error)) => {
                    decoder.finished = true;
                    return Some((Err(error), decoder));
                }
                None => decoder.on_end(),
            }
        }
    })
    .boxed()
}

impl Decoder {
    fn on_frame(&mut self, data: &str) {
        let data = data.trim();
        if data.is_empty() {
            return;
        }
        if data == "[DONE]" {
            self.on_end();
            return;
        }
        let chunk: ChatChunk = match serde_json::from_str(data) {
            Ok(chunk) => chunk,
            Err(error) => {
                self.fail(format!("xAI sent an undecodable stream frame: {error}"));
                return;
            }
        };
        if let Some(usage) = chunk.usage {
            // xAI repeats usage on the terminal frame of some models; one
            // `Usage` chunk per call keeps the engine's accumulation additive.
            if !self.usage_seen {
                self.usage_seen = true;
                self.pending.push_back(Ok(StreamChunk::Usage(usage.into())));
            }
        }
        for choice in chunk.choices {
            self.on_choice(choice);
        }
    }

    fn on_choice(&mut self, choice: Choice) {
        if let Some(text) = choice.delta.reasoning_content.filter(|t| !t.is_empty()) {
            self.pending.push_back(Ok(StreamChunk::ThinkingDelta(text)));
        }
        if let Some(text) = choice.delta.content.filter(|t| !t.is_empty()) {
            self.pending.push_back(Ok(StreamChunk::TextDelta(text)));
        }
        for delta in choice.delta.tool_calls.unwrap_or_default() {
            if self.on_tool_call(delta).is_err() {
                return;
            }
        }
        if let Some(reason) = choice.finish_reason {
            self.close_calls();
            self.stop = Some(stop_reason(&reason));
        }
    }

    fn on_tool_call(&mut self, delta: ToolCallDelta) -> Result<(), ()> {
        let (name, arguments) = delta
            .function
            .map_or((None, None), |f| (f.name, f.arguments));
        if let Some(id) = delta.id.filter(|id| !id.is_empty()) {
            let id = ToolCallId::new(id);
            self.pending.push_back(Ok(StreamChunk::ToolCallStart {
                id: id.clone(),
                name: name.unwrap_or_default(),
            }));
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
            self.fail(format!(
                "xAI sent a tool call fragment for index {} before any call id",
                delta.index
            ));
            return Err(());
        };
        if let Some(arguments) = arguments.filter(|a| !a.is_empty()) {
            call.arguments.push_str(&arguments);
            self.pending.push_back(Ok(StreamChunk::ToolCallArgsDelta {
                id: call.id.clone(),
                delta: arguments,
            }));
        }
        Ok(())
    }

    /// `ToolCallEnd` promises parseable arguments, so the accumulated fragments
    /// are checked here rather than left for the dispatcher to trip over.
    fn close_calls(&mut self) {
        for (_, call) in std::mem::take(&mut self.calls) {
            let arguments = call.arguments.trim();
            if !arguments.is_empty()
                && let Err(error) = serde_json::from_str::<serde_json::Value>(arguments)
            {
                self.fail(format!(
                    "xAI sent unparseable arguments for tool call {}: {error}",
                    call.id
                ));
                return;
            }
            self.pending
                .push_back(Ok(StreamChunk::ToolCallEnd { id: call.id }));
        }
    }

    fn on_end(&mut self) {
        self.finished = true;
        match self.stop.take() {
            Some(stop) => self.pending.push_back(Ok(StreamChunk::Done(stop))),
            None => self.pending.push_back(Err(ProviderError::Protocol(
                "xAI stream ended without a finish_reason".to_string(),
            ))),
        }
    }

    fn fail(&mut self, message: String) {
        self.pending
            .push_back(Err(ProviderError::Protocol(message)));
    }
}

fn stop_reason(reason: &str) -> StopReason {
    match reason {
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "length" | "max_tokens" => StopReason::MaxTokens,
        "content_filter" => StopReason::Refusal {
            message: "xAI stopped the reply for content filtering".to_string(),
        },
        "cancelled" | "aborted" => StopReason::Cancelled,
        // "stop", plus anything xAI adds later: the reply did end, and failing
        // the turn over an unrecognized label would lose a complete answer.
        other => {
            if other != "stop" {
                tracing::debug!(finish_reason = other, "unrecognized xAI finish_reason");
            }
            StopReason::EndTurn
        }
    }
}
