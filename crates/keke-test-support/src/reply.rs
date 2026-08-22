use serde_json::Value;
use serde_json::json;

use crate::sse::SseFrame;

/// How a scripted reply ends.
///
/// Named for what the model did, not for any one vendor's spelling, because the
/// same reply is rendered into three wire formats that spell it differently.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Stop {
    #[default]
    EndTurn,
    ToolUse,
    MaxTokens,
}

/// Token counts, as the vendors' terminal frames report them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// One piece of a scripted assistant turn, in emission order.
#[derive(Clone, Debug)]
pub(crate) enum Part {
    Text(Vec<String>),
    Thinking(Vec<String>),
    /// `frames` are the argument JSON split as it will arrive on the wire.
    /// Providers routinely mis-assemble arguments that span frames, so the
    /// split is part of the scripted intent rather than a rendering detail.
    ToolCall {
        id: String,
        name: String,
        frames: Vec<String>,
    },
}

/// A scripted turn, before it is rendered into a wire format.
#[derive(Clone, Debug, Default)]
pub(crate) struct Script {
    pub(crate) parts: Vec<Part>,
    pub(crate) usage: Option<Usage>,
    pub(crate) stop: Option<Stop>,
    /// Drop the terminal frames, leaving the stream ending mid-turn. This is
    /// the only way to exercise a provider's "stream ended without a terminal
    /// chunk" path, which otherwise only shows up against a flaky network.
    pub(crate) truncated: bool,
}

impl Script {
    /// The stop reason to render. A turn that called tools stops for tool use
    /// unless the script says otherwise, so the common case needs no annotation.
    pub(crate) fn stop(&self) -> Stop {
        self.stop.unwrap_or_else(|| {
            if self
                .parts
                .iter()
                .any(|p| matches!(p, Part::ToolCall { .. }))
            {
                Stop::ToolUse
            } else {
                Stop::EndTurn
            }
        })
    }

    pub(crate) fn usage(&self) -> Usage {
        self.usage.unwrap_or_default()
    }
}

#[derive(Clone, Debug)]
pub(crate) enum ReplyBody {
    Script(Script),
    /// Frames served verbatim, whatever the endpoint's format normally is.
    RawSse(Vec<SseFrame>),
    Json(Value),
}

/// One scripted reply, expressible in every wire format the mock serves.
///
/// A `Reply` describes *intent* — this text, then this tool call, ending here —
/// and the server renders it as chat-completions deltas, Responses typed
/// events, or Messages typed events depending on which endpoint receives it.
/// That is the crate's reason to exist: one script, three wire formats, so a
/// cross-vendor test asserts on behavior instead of on transcripts.
#[derive(Clone, Debug)]
pub struct Reply {
    pub(crate) status: u16,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: ReplyBody,
}

impl Default for Reply {
    fn default() -> Self {
        Self::empty()
    }
}

impl Reply {
    /// A turn with no content, to be filled in with the `with_*` builders.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            status: 200,
            headers: Vec::new(),
            body: ReplyBody::Script(Script::default()),
        }
    }

    /// A turn of visible text delivered as one delta.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::empty().with_text(text)
    }

    /// A turn of visible text delivered as the given deltas, in order.
    #[must_use]
    pub fn text_deltas<S: Into<String>>(deltas: impl IntoIterator<Item = S>) -> Self {
        Self::empty().with_text_deltas(deltas)
    }

    /// A turn of reasoning text with no visible text.
    #[must_use]
    pub fn thinking(text: impl Into<String>) -> Self {
        Self::empty().with_thinking(text)
    }

    /// A turn that calls one tool, its arguments split across two frames.
    #[must_use]
    pub fn tool_call(name: impl Into<String>, arguments: Value) -> Self {
        Self::empty().with_tool_call(name, arguments)
    }

    /// A non-2xx reply carrying a vendor-shaped error body.
    #[must_use]
    pub fn status(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: ReplyBody::Json(json!({
                "error": {
                    "message": format!("mock error {status}"),
                    "type": "mock_error",
                    "code": status,
                }
            })),
        }
    }

    /// A 200 with this JSON body, bypassing rendering entirely.
    #[must_use]
    pub fn json(body: Value) -> Self {
        Self {
            status: 200,
            headers: Vec::new(),
            body: ReplyBody::Json(body),
        }
    }

    /// The escape hatch: frames served exactly as given, whatever the endpoint.
    /// For wire shapes this crate has no opinion about, and for deliberately
    /// malformed bytes.
    #[must_use]
    pub fn raw_sse(frames: Vec<SseFrame>) -> Self {
        Self {
            status: 200,
            headers: Vec::new(),
            body: ReplyBody::RawSse(frames),
        }
    }

    #[must_use]
    pub fn with_text(self, text: impl Into<String>) -> Self {
        self.push(Part::Text(vec![text.into()]))
    }

    #[must_use]
    pub fn with_text_deltas<S: Into<String>>(self, deltas: impl IntoIterator<Item = S>) -> Self {
        self.push(Part::Text(deltas.into_iter().map(Into::into).collect()))
    }

    #[must_use]
    pub fn with_thinking(self, text: impl Into<String>) -> Self {
        self.push(Part::Thinking(vec![text.into()]))
    }

    #[must_use]
    pub fn with_thinking_deltas<S: Into<String>>(self, deltas: impl IntoIterator<Item = S>) -> Self {
        self.push(Part::Thinking(deltas.into_iter().map(Into::into).collect()))
    }

    /// Add a tool call whose serialized arguments are split across two frames.
    #[must_use]
    pub fn with_tool_call(self, name: impl Into<String>, arguments: Value) -> Self {
        let frames = split_in_two(&arguments.to_string());
        self.with_tool_call_frames(name, frames)
    }

    /// Add a tool call whose argument frames are given verbatim, for scripting
    /// a split at a chosen byte — mid-key, mid-escape, or an empty frame.
    #[must_use]
    pub fn with_tool_call_frames<S: Into<String>>(
        self,
        name: impl Into<String>,
        frames: impl IntoIterator<Item = S>,
    ) -> Self {
        let index = self
            .script_parts()
            .iter()
            .filter(|p| matches!(p, Part::ToolCall { .. }))
            .count()
            + 1;
        self.push(Part::ToolCall {
            id: format!("call_{index}"),
            name: name.into(),
            frames: frames.into_iter().map(Into::into).collect(),
        })
    }

    #[must_use]
    pub fn with_usage(mut self, input_tokens: u64, output_tokens: u64) -> Self {
        if let ReplyBody::Script(script) = &mut self.body {
            script.usage = Some(Usage {
                input_tokens,
                output_tokens,
            });
        }
        self
    }

    #[must_use]
    pub fn with_stop(mut self, stop: Stop) -> Self {
        if let ReplyBody::Script(script) = &mut self.body {
            script.stop = Some(stop);
        }
        self
    }

    /// End the stream before its terminal frames, as a dropped connection does.
    #[must_use]
    pub fn truncated(mut self) -> Self {
        if let ReplyBody::Script(script) = &mut self.body {
            script.truncated = true;
        }
        self
    }

    #[must_use]
    pub fn with_status(mut self, status: u16) -> Self {
        self.status = status;
        self
    }

    /// # Panics
    /// If the name or value is not a legal HTTP header, so a typo fails at the
    /// scripting call site rather than inside the server task.
    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        let (name, value) = (name.into(), value.into());
        if axum::http::HeaderName::try_from(name.as_str()).is_err()
            || axum::http::HeaderValue::try_from(value.as_str()).is_err()
        {
            panic!("scripted header `{name}: {value}` is not a legal HTTP header");
        }
        self.headers.push((name, value));
        self
    }

    fn script_parts(&self) -> &[Part] {
        match &self.body {
            ReplyBody::Script(script) => &script.parts,
            _ => &[],
        }
    }

    fn push(mut self, part: Part) -> Self {
        match &mut self.body {
            ReplyBody::Script(script) => script.parts.push(part),
            other => {
                *other = ReplyBody::Script(Script {
                    parts: vec![part],
                    ..Script::default()
                });
            }
        }
        self
    }
}

/// Split at the midpoint, nudged to the next char boundary.
fn split_in_two(text: &str) -> Vec<String> {
    if text.len() < 2 {
        return vec![text.to_owned()];
    }
    let mut mid = text.len() / 2;
    while !text.is_char_boundary(mid) {
        mid += 1;
    }
    vec![text[..mid].to_owned(), text[mid..].to_owned()]
}
