use std::sync::Arc;

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
use keke_protocol::StopReason;
use keke_protocol::ToolCall;
use keke_protocol::ToolCallId;
use keke_protocol::ToolResult;
use keke_provider_api::ModelProvider;
use keke_provider_api::ModelRequest;
use keke_provider_api::ProviderError;
use keke_provider_api::StreamChunk;
use keke_provider_api::ToolSpec;
use serde_json::json;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;

use super::Endpoint;
use super::GrokProvider;

/// Counts calls so the per-request header rule can be asserted directly.
#[derive(Default)]
struct StubAuth {
    calls: std::sync::atomic::AtomicUsize,
}

impl AuthProvider for StubAuth {
    fn id(&self) -> &'static str {
        "grok"
    }

    fn snapshot(&self) -> CredentialSnapshot {
        CredentialSnapshot {
            auth_id: "grok".to_string(),
            source: "test".to_string(),
            ..CredentialSnapshot::default()
        }
    }

    fn headers(&self) -> AuthFuture<'_, Result<AuthHeaders, AuthError>> {
        let seen = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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

fn sse(frames: &[&str]) -> String {
    frames
        .iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect()
}

fn stream_response(body: String) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

async fn provider_over(server: &MockServer) -> (GrokProvider, Arc<StubAuth>) {
    let auth = Arc::new(StubAuth::default());
    let provider = GrokProvider::new(
        auth.clone(),
        Endpoint {
            base_url: format!("{}/v1", server.uri()),
            ..Endpoint::default()
        },
        // No cache: these assert what the endpoint said, and a cache would
        // make the second test in a run assert the first one's answer.
        None,
    );
    (provider, auth)
}

fn request() -> ModelRequest {
    ModelRequest {
        model: "grok-4.6".to_string(),
        messages: vec![Message::user("hi")],
        ..ModelRequest::default()
    }
}

async fn collect(provider: &GrokProvider) -> Vec<Result<StreamChunk, ProviderError>> {
    provider
        .stream(request())
        .await
        .expect("stream starts")
        .collect()
        .await
}

async fn collect_ok(provider: &GrokProvider) -> Vec<StreamChunk> {
    collect(provider)
        .await
        .into_iter()
        .map(|chunk| chunk.expect("no stream error"))
        .collect()
}

#[tokio::test]
async fn text_deltas_assemble_and_end_with_one_done() {
    let server = MockServer::start().await;
    let body = sse(&[
        &json!({"choices":[{"delta":{"content":"Hel"}}]}).to_string(),
        &json!({"choices":[{"delta":{"reasoning_content":"pondering"}}]}).to_string(),
        &json!({"choices":[{"delta":{"content":"lo"}}]}).to_string(),
        &json!({"choices":[{"delta":{},"finish_reason":"stop"}]}).to_string(),
        &json!({"choices":[],"usage":{"prompt_tokens":7,"completion_tokens":2}}).to_string(),
        "[DONE]",
    ]);
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer token-0"))
        .respond_with(stream_response(body))
        .mount(&server)
        .await;

    let (provider, _auth) = provider_over(&server).await;
    let chunks = collect_ok(&provider).await;

    assert_eq!(
        chunks,
        vec![
            StreamChunk::TextDelta("Hel".to_string()),
            StreamChunk::ThinkingDelta("pondering".to_string()),
            StreamChunk::TextDelta("lo".to_string()),
            StreamChunk::Usage(keke_protocol::Usage {
                input_tokens: 7,
                output_tokens: 2,
                ..keke_protocol::Usage::default()
            }),
            StreamChunk::Done(StopReason::EndTurn),
        ]
    );
    assert_eq!(
        chunks
            .iter()
            .filter(|chunk| matches!(chunk, StreamChunk::Done(_)))
            .count(),
        1
    );
}

#[tokio::test]
async fn a_tool_call_split_across_frames_reassembles() {
    let server = MockServer::start().await;
    let body = sse(&[
        &json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"id":"call_1","type":"function","function":{"name":"read_file","arguments":""}}
        ]}}]})
        .to_string(),
        &json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"function":{"arguments":"{\"path\":"}}
        ]}}]})
        .to_string(),
        &json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"function":{"arguments":"\"a.rs\"}"}}
        ]}}]})
        .to_string(),
        &json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}).to_string(),
        "[DONE]",
    ]);
    Mock::given(method("POST"))
        .respond_with(stream_response(body))
        .mount(&server)
        .await;

    let (provider, _auth) = provider_over(&server).await;
    let id = ToolCallId::new("call_1");

    assert_eq!(
        collect_ok(&provider).await,
        vec![
            StreamChunk::ToolCallStart {
                id: id.clone(),
                name: "read_file".to_string()
            },
            StreamChunk::ToolCallArgsDelta {
                id: id.clone(),
                delta: "{\"path\":".to_string()
            },
            StreamChunk::ToolCallArgsDelta {
                id: id.clone(),
                delta: "\"a.rs\"}".to_string()
            },
            StreamChunk::ToolCallEnd { id },
            StreamChunk::Done(StopReason::ToolUse),
        ]
    );
}

#[tokio::test]
async fn a_stream_without_a_finish_reason_is_retryable() {
    let server = MockServer::start().await;
    let body = sse(&[
        &json!({"choices":[{"delta":{"content":"half an answ"}}]}).to_string(),
        "[DONE]",
    ]);
    Mock::given(method("POST"))
        .respond_with(stream_response(body))
        .mount(&server)
        .await;

    let (provider, _auth) = provider_over(&server).await;
    let chunks = collect(&provider).await;

    assert_eq!(chunks.len(), 2);
    assert!(matches!(chunks[0], Ok(StreamChunk::TextDelta(_))));
    assert!(
        matches!(chunks[1], Err(ProviderError::Transient(_))),
        "expected Transient, got {:?}",
        chunks[1]
    );
}

#[tokio::test]
async fn a_truncated_stream_never_reports_done() {
    let server = MockServer::start().await;
    let body = sse(&[&json!({"choices":[{"delta":{"content":"cut"}}]}).to_string()]);
    Mock::given(method("POST"))
        .respond_with(stream_response(body))
        .mount(&server)
        .await;

    let (provider, _auth) = provider_over(&server).await;
    let chunks = collect(&provider).await;

    assert!(
        !chunks
            .iter()
            .any(|chunk| matches!(chunk, Ok(StreamChunk::Done(_))))
    );
    assert!(matches!(
        chunks.last(),
        Some(Err(ProviderError::Transient(_)))
    ));
}

#[tokio::test]
async fn unparseable_tool_arguments_are_a_protocol_error() {
    let server = MockServer::start().await;
    let body = sse(&[
        &json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"id":"call_1","function":{"name":"t","arguments":"{\"a\":"}}
        ]}}]})
        .to_string(),
        &json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}).to_string(),
        "[DONE]",
    ]);
    Mock::given(method("POST"))
        .respond_with(stream_response(body))
        .mount(&server)
        .await;

    let (provider, _auth) = provider_over(&server).await;
    let chunks = collect(&provider).await;

    assert!(matches!(
        chunks.last(),
        Some(Err(ProviderError::Protocol(_)))
    ));
}

#[tokio::test]
async fn rate_limiting_carries_the_stated_delay() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "3")
                .set_body_string("slow down"),
        )
        .mount(&server)
        .await;

    let (provider, _auth) = provider_over(&server).await;
    let error = provider
        .stream(request())
        .await
        .err()
        .expect("rate limited");

    assert!(
        matches!(
            error,
            ProviderError::RateLimited {
                retry_after_millis: Some(3000)
            }
        ),
        "got {error:?}"
    );
    assert!(error.is_retryable());
}

#[tokio::test]
async fn rejected_credentials_are_unauthorized_not_transient() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_json(json!({"error": {"message": "Incorrect API key"}})),
        )
        .mount(&server)
        .await;

    let (provider, _auth) = provider_over(&server).await;
    let error = provider
        .stream(request())
        .await
        .err()
        .expect("unauthorized");

    assert!(
        matches!(error, ProviderError::Unauthorized(ref detail) if detail.contains("Incorrect API key")),
        "got {error:?}"
    );
    assert!(error.needs_reauth());
}

#[tokio::test]
async fn a_malformed_request_is_not_retried_but_a_server_fault_is() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": "bad model"})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(503).set_body_string("upstream down"))
        .mount(&server)
        .await;

    let (provider, _auth) = provider_over(&server).await;

    let invalid = provider.stream(request()).await.err().expect("invalid");
    assert!(matches!(invalid, ProviderError::InvalidRequest(_)));
    assert!(!invalid.is_retryable());

    // Listing does not fail: an endpoint that is down leaves the person with
    // the compiled-in catalog rather than with no picker at all.
    let listed = provider.list_models().await.expect("a catalog either way");
    assert!(listed.iter().any(|model| model.id == "grok-4.6"));
}

#[tokio::test]
async fn credentials_are_fetched_again_for_every_request() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header("authorization", "Bearer token-0"))
        .respond_with(stream_response(sse(&[
            &json!({"choices":[{"delta":{},"finish_reason":"stop"}]}).to_string(),
            "[DONE]",
        ])))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(header("authorization", "Bearer token-1"))
        .respond_with(stream_response(sse(&[
            &json!({"choices":[{"delta":{},"finish_reason":"stop"}]}).to_string(),
            "[DONE]",
        ])))
        .mount(&server)
        .await;

    let (provider, auth) = provider_over(&server).await;
    collect_ok(&provider).await;
    collect_ok(&provider).await;

    assert_eq!(auth.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[tokio::test]
async fn models_are_listed_from_the_models_endpoint() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [
                {"id": "grok-4.6", "input_modalities": ["text", "image"]},
                {"id": "grok-3-mini"}
            ]
        })))
        .mount(&server)
        .await;

    let (provider, _auth) = provider_over(&server).await;
    let models = provider.list_models().await.expect("listed");

    assert_eq!(models.len(), 2);
    assert_eq!(models[0].id, "grok-4.6");
    assert!(models[0].supports_vision);
    assert!(!models[1].supports_vision);
    assert!(models[1].supports_tools);
}

/// The provider is a route and a credential, not a translator: a request it
/// was handed reaches the endpoint as given. Asserted through the wire rather
/// than on the body function, because dropping the field between the two is
/// exactly the failure this guards.
#[tokio::test]
async fn a_configured_effort_reaches_the_endpoint() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(stream_response(sse(&[
            &json!({"choices":[{"delta":{"content":"hi"}}]}).to_string(),
        ])))
        .mount(&server)
        .await;
    let (provider, _auth) = provider_over(&server).await;

    provider
        .stream(ModelRequest {
            reasoning_effort: Some(keke_protocol::ReasoningEffort::XHigh),
            ..request()
        })
        .await
        .expect("stream starts")
        .collect::<Vec<_>>()
        .await;

    let requests = server.received_requests().await.expect("recorded");
    let body: serde_json::Value =
        serde_json::from_slice(&requests[0].body).expect("a JSON request body");
    assert_eq!(body["reasoning_effort"], json!("xhigh"));
}

#[test]
fn provider_info_names_its_route_and_credentials() {
    let provider = GrokProvider::new(Arc::new(StubAuth::default()), Endpoint::default(), None);
    let info = provider.info();

    assert_eq!(info.route, "grok");
    assert_eq!(info.display_name, "xAI Grok");
    assert_eq!(info.auth_id.as_deref(), Some("grok"));
    assert_eq!(info.env_key.as_deref(), Some("XAI_API_KEY"));
    assert_eq!(info.base_url, "https://api.x.ai/v1");
    assert_eq!(info.wire_api, keke_provider_api::WireApi::ChatCompletions);
}

#[test]
fn a_tool_result_becomes_its_own_tool_message() {
    let call_id = ToolCallId::new("call_9");
    let request = ModelRequest {
        model: "grok-4.6".to_string(),
        system: Some("be terse".to_string()),
        messages: vec![
            Message::user("read it"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::thinking("need the file"),
                    ContentBlock::text("Looking."),
                    ContentBlock::ToolCall(ToolCall {
                        id: call_id.clone(),
                        name: "read_file".to_string(),
                        arguments: json!({"path": "a.rs"}),
                    }),
                ],
            },
            Message {
                role: Role::Tool,
                content: vec![ContentBlock::ToolResult(ToolResult::ok(
                    call_id.clone(),
                    "fn main() {}",
                ))],
            },
        ],
        tools: vec![ToolSpec {
            name: "read_file".to_string(),
            description: "read a file".to_string(),
            input_schema: json!({"type": "object"}),
        }],
        max_output_tokens: Some(256),
        temperature: Some(0.5),
        reasoning_effort: None,
    };

    let body = keke_wire::chat_completions_body(&request, true);

    assert_eq!(body["stream"], json!(true));
    assert_eq!(body["stream_options"]["include_usage"], json!(true));
    assert_eq!(body["max_tokens"], json!(256));
    assert_eq!(body["tools"][0]["function"]["name"], json!("read_file"));

    let messages = body["messages"].as_array().expect("messages");
    assert_eq!(messages[0]["role"], json!("system"));
    assert_eq!(messages[0]["content"], json!("be terse"));
    assert_eq!(messages[1]["content"][0]["text"], json!("read it"));
    assert_eq!(messages[2]["role"], json!("assistant"));
    assert_eq!(messages[2]["content"], json!("Looking."));
    assert_eq!(messages[2]["reasoning_content"], json!("need the file"));
    assert_eq!(messages[2]["tool_calls"][0]["id"], json!("call_9"));
    assert_eq!(
        messages[2]["tool_calls"][0]["function"]["arguments"],
        json!(r#"{"path":"a.rs"}"#)
    );
    assert_eq!(messages[3]["role"], json!("tool"));
    assert_eq!(messages[3]["tool_call_id"], json!("call_9"));
    assert_eq!(messages[3]["content"], json!("fn main() {}"));
}

#[test]
fn an_image_travels_as_a_data_uri() {
    let request = ModelRequest {
        model: "grok-4.6".to_string(),
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Image(keke_protocol::ImageBlock {
                data: "AAAA".to_string(),
                media_type: "image/png".to_string(),
            })],
        }],
        ..ModelRequest::default()
    };

    let body = keke_wire::chat_completions_body(&request, false);

    assert_eq!(
        body["messages"][0]["content"][0]["image_url"]["url"],
        json!("data:image/png;base64,AAAA")
    );
    assert_eq!(body["stream"], json!(false));
    assert!(body.get("stream_options").is_none());
}

/// The subscription proxy's listing is the one worth having: it carries the
/// ladder, and dropping it at this seam is what would leave a picker showing
/// bare ids.
#[tokio::test]
async fn the_subscription_listing_reaches_the_caller_with_its_levels() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{
                "id": "grok-4.6",
                "name": "Grok 4.6",
                "context_window": 500_000,
                "reasoning_effort": "high",
                "reasoning_efforts": [
                    {"value": "xhigh"},
                    {"value": "high", "default": true},
                    {"value": "low"}
                ]
            }]
        })))
        .mount(&server)
        .await;

    let (provider, _auth) = provider_over(&server).await;
    let models = provider.list_models().await.expect("listed");

    assert_eq!(models[0].display_name, "Grok 4.6");
    assert_eq!(
        models[0].reasoning_efforts,
        vec![
            keke_protocol::ReasoningEffort::Low,
            keke_protocol::ReasoningEffort::High,
            keke_protocol::ReasoningEffort::XHigh,
        ]
    );
    assert_eq!(
        models[0].starting_effort(),
        Some(keke_protocol::ReasoningEffort::High)
    );
}

fn cached_provider(server: &MockServer, home: &tempfile::TempDir) -> GrokProvider {
    let path = keke_paths::AbsPath::new(home.path()).expect("absolute");
    GrokProvider::new(
        Arc::new(StubAuth::default()),
        Endpoint {
            base_url: format!("{}/v1", server.uri()),
            ..Endpoint::default()
        },
        Some(keke_catalog::CatalogCache::new(
            &path,
            std::time::Duration::from_secs(3600),
        )),
    )
}

/// The point of the cache: opening the interface twice costs one request.
#[tokio::test]
async fn a_cached_catalog_is_answered_without_asking_the_vendor_again() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "grok-4.6", "name": "Grok 4.6"}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let home = tempfile::tempdir().expect("temp dir");

    let first = cached_provider(&server, &home)
        .list_models()
        .await
        .expect("listed");
    let second = cached_provider(&server, &home)
        .list_models()
        .await
        .expect("listed");

    assert_eq!(first, second);
    assert_eq!(first[0].display_name, "Grok 4.6");
    // `expect(1)` above is the assertion: a second request would fail the mock.
}

/// A vendor that has gone away must not empty a picker that was working a
/// minute ago.
#[tokio::test]
async fn an_endpoint_that_fails_falls_back_to_what_it_last_said() {
    let good = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "grok-4.6-preview", "name": "Grok 4.6 Preview"}]
        })))
        .mount(&good)
        .await;
    let home = tempfile::tempdir().expect("temp dir");
    cached_provider(&good, &home)
        .list_models()
        .await
        .expect("listed");

    let broken = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(503).set_body_string("gone"))
        .mount(&broken)
        .await;

    let models = cached_provider(&broken, &home)
        .list_models()
        .await
        .expect("a catalog either way");
    assert_eq!(models[0].id, "grok-4.6-preview");
}

/// And with nothing cached, the compiled-in catalog — never an empty list,
/// which is what would make the surface offer no choice at all.
#[tokio::test]
async fn an_endpoint_that_fails_with_nothing_cached_falls_back_to_the_bundled_catalog() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(401).set_body_string("not signed in"))
        .mount(&server)
        .await;
    let home = tempfile::tempdir().expect("temp dir");

    let models = cached_provider(&server, &home)
        .list_models()
        .await
        .expect("a catalog either way");
    assert!(models.iter().any(|model| model.id == "grok-4.6"));
    assert!(models[0].supports_reasoning());
}
