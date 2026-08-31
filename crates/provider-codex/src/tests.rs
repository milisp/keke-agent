use std::sync::Arc;

use futures::StreamExt;
use keke_auth_api::AuthError;
use keke_auth_api::AuthFuture;
use keke_auth_api::AuthHeaders;
use keke_auth_api::AuthProvider;
use keke_auth_api::CredentialSnapshot;
use keke_auth_api::LoginUi;
use keke_config_types::WebSearchConfig;
use keke_config_types::WebSearchMode;
use keke_provider_api::ModelProvider;
use keke_provider_api::StreamChunk;
use serde_json::json;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;
use wiremock::matchers::query_param;

use super::CodexProvider;
use super::Endpoint;

#[derive(Default)]
struct StubAuth;

impl AuthProvider for StubAuth {
    fn id(&self) -> &'static str {
        "codex"
    }

    fn snapshot(&self) -> CredentialSnapshot {
        CredentialSnapshot {
            auth_id: "codex".to_string(),
            source: "test".to_string(),
            ..CredentialSnapshot::default()
        }
    }

    fn headers(&self) -> AuthFuture<'_, Result<AuthHeaders, AuthError>> {
        Box::pin(async { Ok(AuthHeaders::bearer("token")) })
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

fn provider_over(server: &MockServer, home: Option<&tempfile::TempDir>) -> CodexProvider {
    let cache = home.map(|home| {
        let path = keke_paths::AbsPath::new(home.path()).expect("absolute");
        keke_catalog::CatalogCache::new(&path, std::time::Duration::from_secs(3600))
    });
    CodexProvider::new(
        Arc::new(StubAuth),
        Endpoint {
            base_url: format!("{}/backend-api/codex", server.uri()),
            fixed_sampling: true,
            ..Endpoint::default()
        },
        cache,
    )
}

#[test]
fn provider_info_names_its_route_and_credentials() {
    let provider = CodexProvider::new(
        Arc::new(StubAuth),
        Endpoint {
            base_url: "https://chatgpt.com/backend-api/codex".to_string(),
            fixed_sampling: true,
            ..Endpoint::default()
        },
        None,
    );
    let info = provider.info();

    assert_eq!(info.route, "codex");
    assert_eq!(info.auth_id.as_deref(), Some("codex"));
    assert_eq!(info.env_key.as_deref(), Some("OPENAI_API_KEY"));
    assert_eq!(info.wire_api, keke_provider_api::WireApi::Responses);
}

/// The listing the ChatGPT backend actually sends, end to end: what a picker
/// gets must be names and ladders, not slugs.
#[tokio::test]
async fn the_subscription_listing_reaches_the_caller_with_its_levels() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/backend-api/codex/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "models": [{
                "slug": "gpt-5.6-sol",
                "display_name": "GPT-5.6-Sol",
                "description": "Latest frontier agentic coding model.",
                "context_window": 272_000,
                "visibility": "list",
                "default_reasoning_level": "low",
                "supported_reasoning_levels": [
                    {"effort": "low"}, {"effort": "high"}, {"effort": "ultra"}
                ]
            }]
        })))
        .mount(&server)
        .await;

    let models = provider_over(&server, None)
        .list_models()
        .await
        .expect("listed");

    assert_eq!(models[0].display_name, "GPT-5.6-Sol");
    assert_eq!(
        models[0].reasoning_efforts,
        vec![
            keke_protocol::ReasoningEffort::Low,
            keke_protocol::ReasoningEffort::High,
            keke_protocol::ReasoningEffort::Ultra,
        ]
    );
}

/// The listing is gated on `client_version`: without the query parameter the
/// backend refuses the request outright, and with one it considers too old it
/// answers `{"models":[]}`. Both land in the fallback *without* filling the
/// cache, so every launch pays for a round trip whose result is discarded —
/// which is the slow start this asserts against.
#[tokio::test]
async fn the_listing_names_the_client_version_the_backend_gates_on() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/backend-api/codex/models"))
        .and(query_param("client_version", super::DEFAULT_CLIENT_VERSION))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "models": [{"slug": "gpt-5.6-luna", "display_name": "GPT-5.6-Luna"}]
        })))
        .mount(&server)
        .await;

    let models = provider_over(&server, None)
        .list_models()
        .await
        .expect("listed");

    // The bundled fallback would answer too, so the assertion is that the
    // *vendor* answered: only a request carrying the version matches the mock.
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "gpt-5.6-luna");
}

/// The failure this whole change exists to fix: the ChatGPT backend has no
/// OpenAI-style model listing, and a 404 there used to reach the surface as
/// "this provider serves nothing".
#[tokio::test]
async fn an_endpoint_with_no_listing_still_yields_a_picker() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
        .mount(&server)
        .await;

    let models = provider_over(&server, None)
        .list_models()
        .await
        .expect("a catalog either way");

    assert!(models.iter().any(|model| model.id == "gpt-5.5"));
    assert!(models[0].supports_reasoning());
}

/// A hosted tool is one the vendor runs without the harness seeing it, so it
/// is offered only when a deployment said so.
#[test]
fn no_search_tool_is_offered_unless_a_deployment_asks_for_one() {
    assert_eq!(super::web_search::tool(&WebSearchConfig::default()), None);
}

/// The three access levels are three different requests, not one flag: cached
/// forbids live fetches outright, indexed permits them but only to pages the
/// vendor already holds, live permits them everywhere.
#[test]
fn each_access_level_asks_the_vendor_for_a_different_reach() {
    let of = |mode| super::web_search::tool(&WebSearchConfig::enabled(mode)).expect("a tool");

    let cached = of(WebSearchMode::Cached);
    assert_eq!(cached["type"], "web_search");
    assert_eq!(cached["external_web_access"], json!(false));
    assert!(cached.get("indexed_web_access").is_none());

    let indexed = of(WebSearchMode::Indexed);
    assert_eq!(indexed["external_web_access"], json!(true));
    assert_eq!(indexed["indexed_web_access"], json!(true));

    let live = of(WebSearchMode::Live);
    assert_eq!(live["external_web_access"], json!(true));
    assert!(live.get("indexed_web_access").is_none());
}

/// A deployment that may only consult approved sources has nowhere else to say
/// so, since the search never reaches the approval seam.
#[test]
fn a_confined_search_names_its_domains_and_its_locale() {
    let tool = super::web_search::tool(&WebSearchConfig {
        mode: WebSearchMode::Live,
        context_size: keke_config_types::WebSearchContextSize::High,
        allowed_domains: vec!["docs.rs".to_string(), "rust-lang.org".to_string()],
        user_location: Some(keke_config_types::WebSearchLocation {
            country: Some("US".to_string()),
            city: Some("San Francisco".to_string()),
            ..keke_config_types::WebSearchLocation::default()
        }),
        include_images: true,
    })
    .expect("a tool");

    assert_eq!(
        tool["filters"]["allowed_domains"],
        json!(["docs.rs", "rust-lang.org"])
    );
    assert_eq!(tool["search_context_size"], "high");
    assert_eq!(tool["user_location"]["type"], "approximate");
    assert_eq!(tool["user_location"]["country"], "US");
    assert_eq!(tool["user_location"]["city"], "San Francisco");
    assert!(tool["user_location"].get("region").is_none());
    assert_eq!(tool["search_content_types"], json!(["text", "image"]));
}

/// The request the model actually sees: the hosted tool travels in the same
/// list as the harness's own tools, and after them, so it can never displace
/// one keke advertised.
#[tokio::test]
async fn the_search_tool_reaches_the_endpoint_alongside_the_harness_tools() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let provider = CodexProvider::new(
        Arc::new(StubAuth),
        Endpoint {
            base_url: format!("{}/backend-api/codex", server.uri()),
            web_search: WebSearchConfig::enabled(WebSearchMode::Live),
            ..Endpoint::default()
        },
        None,
    );

    let _ = provider
        .stream(keke_provider_api::ModelRequest {
            model: "gpt-5.6-luna".to_string(),
            reasoning_effort: Some(keke_protocol::ReasoningEffort::Low),
            tools: vec![keke_provider_api::ToolSpec {
                name: "shell".to_string(),
                description: "run a command".to_string(),
                input_schema: json!({"type": "object"}),
            }],
            ..keke_provider_api::ModelRequest::default()
        })
        .await;

    let request = &server.received_requests().await.expect("recorded")[0];
    let body: serde_json::Value = serde_json::from_slice(&request.body).expect("json");
    let tools = body["tools"].as_array().expect("tools");
    assert_eq!(tools[0]["name"], "shell");
    assert_eq!(tools[1]["type"], "web_search");
}

/// The inverse, and the one that costs money if it is wrong: an instance that
/// was never configured for search sends no search tool.
#[tokio::test]
async fn an_unconfigured_instance_sends_no_search_tool() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let _ = provider_over(&server, None)
        .stream(keke_provider_api::ModelRequest {
            model: "gpt-5.6-luna".to_string(),
            reasoning_effort: Some(keke_protocol::ReasoningEffort::Low),
            ..keke_provider_api::ModelRequest::default()
        })
        .await;

    let request = &server.received_requests().await.expect("recorded")[0];
    let body: serde_json::Value = serde_json::from_slice(&request.body).expect("json");
    assert!(body.get("tools").is_none());
}

/// The search itself runs at the vendor, invisibly to the harness's own tool
/// dispatch — but the fact that it ran, and what it asked, must still land on
/// the durable record (`AGENTS.md` invariant 6). This asserts that the wire
/// decoder surfaces a `web_search_call` item as a `StreamChunk`, rather than
/// silently dropping it the way an unrecognized item kind otherwise would.
#[tokio::test]
async fn a_hosted_search_call_is_surfaced_as_a_stream_chunk() {
    let server = MockServer::start().await;
    let frames = [
        json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"type": "web_search_call", "id": "ws_1", "status": "in_progress"},
        }),
        json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "type": "web_search_call",
                "id": "ws_1",
                "status": "completed",
                "action": {"type": "search", "query": "rust async traits"},
            },
        }),
        json!({
            "type": "response.completed",
            "response": {"usage": {"input_tokens": 1, "output_tokens": 1}},
        }),
    ];
    let body: String = frames
        .iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect();
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(&server)
        .await;
    let provider = CodexProvider::new(
        Arc::new(StubAuth),
        Endpoint {
            base_url: format!("{}/backend-api/codex", server.uri()),
            fixed_sampling: true,
            ..Endpoint::default()
        },
        None,
    );

    let chunks: Vec<StreamChunk> = provider
        .stream(keke_provider_api::ModelRequest {
            model: "gpt-5.6-luna".to_string(),
            ..keke_provider_api::ModelRequest::default()
        })
        .await
        .expect("stream starts")
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(|chunk| chunk.expect("no stream error"))
        .collect();

    assert!(chunks.contains(&StreamChunk::HostedToolCall {
        name: "web_search".to_string(),
        query: Some("rust async traits".to_string()),
    }));
}

#[tokio::test]
async fn a_cached_catalog_is_answered_without_asking_the_vendor_again() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/backend-api/codex/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "models": [{"slug": "gpt-5.6-terra", "display_name": "GPT-5.6-Terra"}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let home = tempfile::tempdir().expect("temp dir");

    let first = provider_over(&server, Some(&home))
        .list_models()
        .await
        .expect("listed");
    let second = provider_over(&server, Some(&home))
        .list_models()
        .await
        .expect("listed");

    assert_eq!(first, second);
    assert_eq!(first[0].id, "gpt-5.6-terra");
}
