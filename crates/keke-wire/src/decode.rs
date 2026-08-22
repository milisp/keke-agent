//! The driver every wire format's SSE decoder runs on.
//!
//! All three formats need the same two properties and get them wrong in the
//! same two ways: a stream must end with exactly one [`StreamChunk::Done`], and
//! once it has produced a terminal chunk nothing may follow it. Putting the
//! bookkeeping here means a new format only has to say what its frames *mean*,
//! not re-derive when a reply is allowed to be considered complete.

use std::collections::VecDeque;

use futures::StreamExt;
use futures::stream::BoxStream;
use keke_protocol::StopReason;
use keke_provider_api::ProviderError;
use keke_provider_api::StreamChunk;

/// The payload of one SSE `data:` field.
pub(crate) type Frame = Result<String, ProviderError>;

/// Where a decoder writes what it has normalized.
///
/// The completion flag is the load-bearing part: after `finish` or `fail` the
/// sink silently drops everything, so a decoder that keeps interpreting frames
/// past a failure cannot turn a truncated reply back into a successful one.
pub(crate) struct Sink {
    queue: VecDeque<Result<StreamChunk, ProviderError>>,
    complete: bool,
}

impl Sink {
    fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            complete: false,
        }
    }

    pub(crate) fn push(&mut self, chunk: StreamChunk) {
        if !self.complete {
            self.queue.push_back(Ok(chunk));
        }
    }

    /// End the stream successfully. The only place `Done` is produced.
    pub(crate) fn finish(&mut self, stop: StopReason) {
        if !self.complete {
            self.complete = true;
            self.queue.push_back(Ok(StreamChunk::Done(stop)));
        }
    }

    /// End the stream with an error the caller must not mistake for a reply.
    pub(crate) fn abort(&mut self, error: ProviderError) {
        if !self.complete {
            self.complete = true;
            self.queue.push_back(Err(error));
        }
    }

    /// End the stream because the provider's own output did not make sense.
    ///
    /// For a frame keke could not read. A reply that simply *stopped* is
    /// [`Sink::truncated`] instead — the two look alike and call for opposite
    /// responses.
    pub(crate) fn fail(&mut self, message: impl Into<String>) {
        self.abort(ProviderError::Protocol(message.into()));
    }

    /// End the stream because it stopped before saying it was finished.
    ///
    /// Reported as transient, which is the more useful reading of an ambiguous
    /// signal: a reply that stops early is far more often a dropped connection
    /// or a proxy cutting a long response than a provider that never sends a
    /// terminal event at all. Calling it a protocol error would tell the engine
    /// not to retry something a retry usually fixes — a real truncated NVIDIA
    /// response is what surfaced this.
    pub(crate) fn truncated(&mut self, message: impl Into<String>) {
        self.abort(ProviderError::Transient(message.into()));
    }

    pub(crate) fn is_complete(&self) -> bool {
        self.complete
    }
}

/// One wire format's reading of its own SSE frames.
pub(crate) trait WireDecoder: Send + 'static {
    /// Interpret one `data:` payload.
    fn on_frame(&mut self, data: &str, out: &mut Sink);

    /// The transport ended. A decoder that holds a stop reason back until the
    /// last moment — most of them do, because usage trails `finish_reason` —
    /// emits it here.
    fn on_end(&mut self, out: &mut Sink);
}

struct Driver<D> {
    frames: BoxStream<'static, Frame>,
    decoder: D,
    sink: Sink,
    drained: bool,
}

/// Run `decoder` over `frames`, yielding neutral chunks.
///
/// The returned stream ends with exactly one [`StreamChunk::Done`] or with a
/// [`ProviderError`]; it never ends silently on a truncated reply.
pub(crate) fn run<D: WireDecoder>(
    frames: BoxStream<'static, Frame>,
    decoder: D,
) -> BoxStream<'static, Result<StreamChunk, ProviderError>> {
    let driver = Driver {
        frames,
        decoder,
        sink: Sink::new(),
        drained: false,
    };
    futures::stream::unfold(driver, |mut driver| async move {
        loop {
            if let Some(chunk) = driver.sink.queue.pop_front() {
                return Some((chunk, driver));
            }
            if driver.drained {
                return None;
            }
            match driver.frames.next().await {
                Some(Ok(data)) => driver.decoder.on_frame(&data, &mut driver.sink),
                // A break mid-stream is a transport failure, not a malformed
                // reply: the engine may retry it.
                Some(Err(error)) => {
                    driver.drained = true;
                    driver.sink.abort(error);
                }
                None => {
                    driver.drained = true;
                    driver.decoder.on_end(&mut driver.sink);
                    driver
                        .sink
                        .truncated("the provider's stream ended without a terminal event");
                }
            }
            // Frames past the terminal chunk are noise; stop reading them.
            if driver.sink.is_complete() {
                driver.drained = true;
            }
        }
    })
    .boxed()
}
