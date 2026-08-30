//! What every wire format must do identically, asserted three times over.
//!
//! The formats are tested separately rather than through a shared table because
//! the interesting part is the translation, and a table would only be able to
//! assert the parts that already look the same.

mod chat_completions;
mod messages;
mod responses;

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use futures::StreamExt;
use keke_auth_api::AuthError;
use keke_auth_api::AuthFuture;
use keke_auth_api::AuthHeaders;
use keke_auth_api::AuthProvider;
use keke_auth_api::CredentialSnapshot;
use keke_auth_api::LoginUi;
use keke_protocol::Message;
use keke_provider_api::ModelRequest;
use keke_provider_api::ProviderError;
use keke_provider_api::StreamChunk;
use keke_provider_api::WireApi;
use serde_json::Value;
use wiremock::MockServer;
use wiremock::ResponseTemplate;

use crate::WireClient;

/// Counts credential fetches so the per-request rule can be asserted directly
/// rather than inferred from a header value.
#[derive(Default)]
pub(super) struct StubAuth {
    calls: AtomicUsize,
}

impl StubAuth {
    fn fetches(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl AuthProvider for StubAuth {
    fn id(&self) -> &'static str {
        "stub"
    }

    fn snapshot(&self) -> CredentialSnapshot {
        CredentialSnapshot {
            auth_id: "stub".to_string(),
            source: "test".to_string(),
            ..CredentialSnapshot::default()
        }
    }

    fn headers(&self) -> AuthFuture<'_, Result<AuthHeaders, AuthError>> {
        let seen = self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { Ok(AuthHeaders::bearer(&format!("token-{seen}"))) })
    }

    fn login<'a>(&'a self, _ui: Arc<dyn LoginUi>) -> AuthFuture<'a, Result<(), AuthError>> {
        Box::pin(async { Ok(()) })
    }

    fn refresh_after_unauthorized(&self) -> AuthFuture<'_, bool> {
        Box::pin(async { false })
    }

    fn logout(&self) -> AuthFuture<'_, Result<(), AuthError>> {
        Box::pin(async { Ok(()) })
    }
}

/// Frame a list of `data:` payloads as an SSE body.
fn sse(frames: &[String]) -> String {
    frames
        .iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect()
}

fn stream_response(body: String) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

fn client_over(server: &MockServer) -> (WireClient, Arc<StubAuth>) {
    let auth = Arc::new(StubAuth::default());
    let client = WireClient::new(format!("{}/v1", server.uri()), auth.clone());
    (client, auth)
}

fn request() -> ModelRequest {
    ModelRequest {
        model: "a-model".to_string(),
        messages: vec![Message::user("hi")],
        ..ModelRequest::default()
    }
}

async fn collect(client: &WireClient, api: WireApi) -> Vec<Result<StreamChunk, ProviderError>> {
    client
        .stream(api, request())
        .await
        .expect("stream starts")
        .collect()
        .await
}

async fn collect_ok(client: &WireClient, api: WireApi) -> Vec<StreamChunk> {
    collect(client, api)
        .await
        .into_iter()
        .map(|chunk| chunk.expect("no stream error"))
        .collect()
}

/// The body of the first request the server saw, as JSON.
async fn sent_body(server: &MockServer) -> Value {
    let requests = server
        .received_requests()
        .await
        .expect("the server records requests");
    serde_json::from_slice(&requests.first().expect("one request").body).expect("a JSON body")
}

/// Every successful stream ends with exactly one `Done`, whatever the format.
fn assert_ends_with_one_done(chunks: &[StreamChunk]) {
    assert!(
        matches!(chunks.last(), Some(StreamChunk::Done(_))),
        "expected a trailing Done, got {chunks:?}"
    );
    assert_eq!(
        chunks
            .iter()
            .filter(|chunk| matches!(chunk, StreamChunk::Done(_)))
            .count(),
        1,
        "expected exactly one Done in {chunks:?}"
    );
}

/// The reassembled arguments of the one tool call in `chunks`.
fn one_tool_call(chunks: &[StreamChunk]) -> (String, String, String) {
    let mut id = String::new();
    let mut name = String::new();
    let mut arguments = String::new();
    let mut starts = 0;
    let mut ends = 0;
    for chunk in chunks {
        match chunk {
            StreamChunk::ToolCallStart { id: got, name: n } => {
                starts += 1;
                id = got.to_string();
                name.clone_from(n);
            }
            StreamChunk::ToolCallArgsDelta { delta, .. } => arguments.push_str(delta),
            StreamChunk::ToolCallEnd { .. } => ends += 1,
            _ => {}
        }
    }
    assert_eq!(starts, 1, "expected one ToolCallStart in {chunks:?}");
    assert_eq!(ends, 1, "expected one ToolCallEnd in {chunks:?}");
    (id, name, arguments)
}
