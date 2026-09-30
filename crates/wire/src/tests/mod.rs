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
use keke_protocol::ContentBlock;
use keke_protocol::Message;
use keke_protocol::Role;
use keke_protocol::ToolCall;
use keke_protocol::ToolCallId;
use keke_protocol::ToolResult;
use keke_provider_api::ModelRequest;
use keke_provider_api::ProviderError;
use keke_provider_api::StreamChunk;
use keke_provider_api::ToolSpec;
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

/// An MCP tool id as the engine knows it, and the name a vendor will accept.
const MCP_ID: &str = "acp:codexia-bots:list_bots";
const MCP_WIRE: &str = "acp__codexia-bots__list_bots";

/// A request that offers an MCP tool beside a builtin and has already called
/// the MCP one, so both places a tool name travels are exercised.
fn mcp_request() -> ModelRequest {
    let call_id = ToolCallId::new("call_mcp");
    let spec = |name: &str| ToolSpec {
        name: name.to_string(),
        description: "a tool".to_string(),
        input_schema: serde_json::json!({"type": "object"}),
    };
    ModelRequest {
        model: "a-model".to_string(),
        messages: vec![
            Message::user("list the bots"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolCall(ToolCall {
                    id: call_id.clone(),
                    name: MCP_ID.to_string(),
                    arguments: serde_json::json!({}),
                })],
            },
            Message {
                role: Role::Tool,
                content: vec![ContentBlock::ToolResult(ToolResult::ok(call_id, "none"))],
            },
        ],
        tools: vec![spec("bash"), spec(MCP_ID)],
        ..ModelRequest::default()
    }
}

/// Every string stored under a `name` key anywhere in `body`. Tool names sit
/// at a different depth in each wire format, and the schemas used here have no
/// property called `name`, so walking the whole body finds them all without
/// encoding any one format's layout.
fn names_in(body: &Value) -> Vec<String> {
    fn walk(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    match child {
                        Value::String(name) if key == "name" => out.push(name.clone()),
                        _ => walk(child, out),
                    }
                }
            }
            Value::Array(items) => items.iter().for_each(|item| walk(item, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(body, &mut out);
    out
}

/// What a vendor enforces: 1 to 64 of `[a-zA-Z0-9_-]`.
fn is_vendor_safe(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

/// The tool names in a body built from [`mcp_request`]: all acceptable to a
/// vendor, `bash` untouched, and the MCP tool renamed in both the definitions
/// and the history (so exactly twice).
fn assert_mcp_names_sanitized(body: &Value) {
    let names = names_in(body);
    for name in &names {
        assert!(is_vendor_safe(name), "{name:?} would be rejected: {body}");
    }
    let count = |wanted: &str| names.iter().filter(|name| *name == wanted).count();
    assert_eq!(count("bash"), 1, "{body}");
    assert_eq!(count(MCP_WIRE), 2, "{body}");
    assert_eq!(count(MCP_ID), 0, "{body}");
}
