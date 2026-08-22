//! A mock inference backend for provider tests.
//!
//! Providers are the part of keke that is hardest to test honestly: the bugs
//! live in the wire format — a tool call whose arguments span frames, a stream
//! that stops before its terminal event, a 429 whose `retry-after` nobody read
//! — and none of those reproduce against a hand-written response fixture.
//!
//! [`MockInferenceServer`] serves all three inference wire formats from one
//! scripted [`Reply`], so the same intent can be asserted against every
//! provider that speaks any of them.
//!
//! ```no_run
//! # use keke_test_support::{Endpoint, MockInferenceServer, Reply};
//! # async fn example() {
//! let server = MockInferenceServer::start().await;
//! server.script(Endpoint::ChatCompletions, Reply::text("hello").with_usage(10, 2));
//! server.script(
//!     Endpoint::Messages,
//!     Reply::tool_call("read_file", serde_json::json!({ "path": "a" })),
//! );
//! let base_url = server.base_url();
//! # }
//! ```

mod reply;
mod server;
mod sse;
mod wire;

pub use reply::Reply;
pub use reply::Stop;
pub use reply::Usage;
pub use server::Endpoint;
pub use server::MockInferenceServer;
pub use server::RecordedRequest;
pub use sse::SseFrame;
