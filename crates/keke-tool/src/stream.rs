//! The tool execution stream and its invariant.
//!
//! A [`ToolStream`] yields `[Progress(_)*, Terminal(_)]` — any number of
//! progress notices followed by exactly one terminal outcome. The constructors
//! are the only way to build one, so the invariant holds by construction on the
//! producing side; the dispatcher enforces it on the consuming side by treating
//! a stream that ends early as a protocol violation.

use futures::Stream;
use futures::StreamExt;
use futures::stream;

use crate::ToolError;

/// One item in a tool's execution stream.
#[derive(Debug)]
pub enum ToolEvent<T> {
    /// An interim notice for surfaces. Never reaches the model.
    Progress(ToolProgress),
    /// The single, final outcome.
    Terminal(Result<T, ToolError>),
}

/// A progress notice.
#[derive(Clone, Debug)]
pub struct ToolProgress {
    /// A short human-readable line, e.g. `"read 120 of 4000 lines"`.
    pub message: String,
    /// Completion in `0.0..=1.0` when the tool can estimate it.
    pub fraction: Option<f32>,
}

impl ToolProgress {
    pub fn message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            fraction: None,
        }
    }
}

/// A tool's execution stream.
pub struct ToolStream<T> {
    inner: stream::BoxStream<'static, ToolEvent<T>>,
}

impl<T: Send + 'static> ToolStream<T> {
    /// A stream with no progress: just the outcome.
    #[must_use]
    pub fn terminal_only(result: Result<T, ToolError>) -> Self {
        Self {
            inner: stream::once(async move { ToolEvent::Terminal(result) }).boxed(),
        }
    }

    /// A stream of progress notices followed by the outcome.
    ///
    /// The terminal item is appended here rather than taken from `progress`, so
    /// a caller cannot accidentally build a stream with two terminals or none.
    #[must_use]
    pub fn with_progress<S>(progress: S, result: Result<T, ToolError>) -> Self
    where
        S: Stream<Item = ToolProgress> + Send + 'static,
    {
        let tail = stream::once(async move { ToolEvent::Terminal(result) });
        Self {
            inner: progress.map(ToolEvent::Progress).chain(tail).boxed(),
        }
    }

    /// Consume the stream, discarding progress and returning the outcome.
    ///
    /// A stream that ends without a terminal is a producer bug, reported as
    /// `tool_stream_no_terminal` rather than silently succeeding.
    pub async fn into_terminal(mut self) -> Result<T, ToolError> {
        let mut last = None;
        while let Some(event) = self.inner.next().await {
            if let ToolEvent::Terminal(result) = event {
                last = Some(result);
            }
        }
        last.unwrap_or_else(|| {
            Err(ToolError::custom(
                "tool_stream_no_terminal",
                "tool stream ended without a terminal item",
            ))
        })
    }
}

impl<T> Stream for ToolStream<T> {
    type Item = ToolEvent<T>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.inner.poll_next_unpin(cx)
    }
}
