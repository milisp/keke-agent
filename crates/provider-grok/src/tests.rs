use std::sync::Arc;

use futures::StreamExt;
use keke_auth_api::AuthError;
use keke_auth_api::AuthFuture;
use keke_auth_api::AuthHeaders;
use keke_auth_api::AuthProvider;
use keke_auth_api::CredentialSnapshot;
use keke_auth_api::LoginUi;
use keke_protocol::Message;
use keke_provider_api::ModelProvider;
use keke_provider_api::ModelRequest;
use keke_provider_api::ProviderError;
use serde_json::json;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
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
    )
    .expect("the default endpoint offers no search");
    (provider, auth)
}

fn request() -> ModelRequest {
    ModelRequest {
        model: "grok-4.6".to_string(),
        messages: vec![Message::user("hi")],
        ..ModelRequest::default()
    }
}

#[tokio::test]
async fn cache_affinity_follows_the_session_on_both_wires() {
    use keke_provider_api::WireApi;

    for (wire_api, endpoint, frame) in [
        (
            WireApi::ChatCompletions,
            "/v1/chat/completions",
            r#"{"choices":[{"index":0,"delta":{"content":"hello"},"finish_reason":"stop"}]}"#,
        ),
        (
            WireApi::Responses,
            "/v1/responses",
            r#"{"type":"response.completed","response":{}}"#,
        ),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(endpoint))
            .respond_with(stream_response(sse(&[frame])))
            .mount(&server)
            .await;
        let provider = GrokProvider::new(
            Arc::new(StubAuth::default()),
            Endpoint {
                base_url: format!("{}/v1", server.uri()),
                wire_api,
                ..Endpoint::default()
            },
            None,
        )
        .expect("provider");
        let session = keke_protocol::SessionId::new();
        let other = keke_protocol::SessionId::new();
        for (session_id, text) in [
            (Some(session), "first turn"),
            (Some(session), "next turn"),
            (Some(other), "another session"),
            (None, "standalone call"),
        ] {
            let chunks: Vec<_> = provider
                .stream(ModelRequest {
                    session_id,
                    messages: vec![Message::user(text)],
                    ..request()
                })
                .await
                .expect("stream starts")
                .collect()
                .await;
            assert!(chunks.iter().all(Result::is_ok));
        }
        let requests = server.received_requests().await.expect("recorded");
        assert_eq!(requests.len(), 4);
        for (index, id) in [session, session, other].into_iter().enumerate() {
            assert_eq!(requests[index].headers["x-grok-conv-id"], id.to_string());
            assert_eq!(requests[index].headers["x-grok-session-id"], id.to_string());
            assert_eq!(
                requests[index].headers["authorization"],
                format!("Bearer token-{index}")
            );
            let body: serde_json::Value =
                serde_json::from_slice(&requests[index].body).expect("json");
            assert!(body.get("x_grok_session_id").is_none());
            if wire_api == WireApi::Responses {
                assert_eq!(body["prompt_cache_key"], id.to_string());
                // Nothing is stored at xAI: reasoning has to come back to be kept.
                assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
            } else {
                assert!(body.get("include").is_none());
            }
        }
        assert!(!requests[3].headers.contains_key("x-grok-session-id"));
        assert!(!requests[3].headers.contains_key("x-grok-conv-id"));
    }
}

#[tokio::test]
async fn a_models_endpoint_that_is_down_leaves_the_compiled_in_catalog() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(503).set_body_string("upstream down"))
        .mount(&server)
        .await;

    let (provider, _auth) = provider_over(&server).await;

    // Listing does not fail: an endpoint that is down leaves the person with
    // the compiled-in catalog rather than with no picker at all.
    let listed = provider.list_models().await.expect("a catalog either way");
    assert!(listed.iter().any(|model| model.id == "grok-4.6"));
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
    let provider = GrokProvider::new(Arc::new(StubAuth::default()), Endpoint::default(), None)
        .expect("the default endpoint offers no search");
    let info = provider.info();

    assert_eq!(info.route, "grok");
    assert_eq!(info.auth_id.as_deref(), Some("grok"));
    assert_eq!(info.env_key.as_deref(), Some("XAI_API_KEY"));
    assert_eq!(info.base_url, "https://api.x.ai/v1");
    assert_eq!(info.wire_api, keke_provider_api::WireApi::ChatCompletions);
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
    .expect("the default endpoint offers no search")
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

mod web_search {
    use super::*;
    use keke_config_types::WebSearchConfig;
    use keke_config_types::WebSearchContextSize;
    use keke_config_types::WebSearchLocation;
    use keke_config_types::WebSearchMode;
    use keke_provider_api::WireApi;

    fn endpoint(web_search: WebSearchConfig, wire_api: WireApi) -> Endpoint {
        Endpoint {
            web_search,
            wire_api,
            ..Endpoint::default()
        }
    }

    fn built(config: WebSearchConfig, wire_api: WireApi) -> Result<GrokProvider, ProviderError> {
        GrokProvider::new(
            Arc::new(StubAuth::default()),
            endpoint(config, wire_api),
            None,
        )
    }

    /// The body the *conversation's* endpoint receives.
    ///
    /// Nothing about the search may appear here: a hosted entry the model
    /// cannot see the point of is what sent it to `curl`, and one it can see
    /// still runs where no guard and no log can reach it.
    async fn turn_body(config: WebSearchConfig, wire_api: WireApi) -> serde_json::Value {
        let server = MockServer::start().await;
        let (route, frame) = match wire_api {
            WireApi::Responses => (
                "/v1/responses",
                json!({"type": "response.output_text.delta", "delta": "hi"}),
            ),
            _ => (
                "/v1/chat/completions",
                json!({"choices":[{"delta":{"content":"hi"}}]}),
            ),
        };
        Mock::given(method("POST"))
            .and(path(route))
            .respond_with(stream_response(sse(&[&frame.to_string()])))
            .mount(&server)
            .await;
        let provider = GrokProvider::new(
            Arc::new(StubAuth::default()),
            Endpoint {
                base_url: format!("{}/v1", server.uri()),
                ..endpoint(config, wire_api)
            },
            None,
        )
        .expect("this configuration is expressible at xAI");

        provider
            .stream(request())
            .await
            .expect("stream starts")
            .collect::<Vec<_>>()
            .await;

        let requests = server.received_requests().await.expect("recorded");
        serde_json::from_slice(&requests[0].body).expect("a JSON request body")
    }

    /// One search, and the request it sent to run it.
    async fn search_body(
        config: WebSearchConfig,
        wire_api: WireApi,
        reply: serde_json::Value,
        query: &str,
        domains: &[String],
    ) -> (
        Result<keke_provider_api::WebSearchResults, ProviderError>,
        serde_json::Value,
    ) {
        let server = MockServer::start().await;
        let route = match wire_api {
            WireApi::Responses => "/v1/responses",
            _ => "/v1/chat/completions",
        };
        Mock::given(method("POST"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_string(reply.to_string()))
            .mount(&server)
            .await;
        let provider = GrokProvider::new(
            Arc::new(StubAuth::default()),
            Endpoint {
                base_url: format!("{}/v1", server.uri()),
                default_model: Some("grok-4.6".to_string()),
                ..endpoint(config, wire_api)
            },
            None,
        )
        .expect("this configuration is expressible at xAI");

        let backend = provider.web_search().expect("a configured search");
        let results = backend.search(query, domains).await;
        let requests = server.received_requests().await.expect("recorded");
        let body = serde_json::from_slice(&requests[0].body).expect("a JSON request body");
        (results, body)
    }

    fn responses_reply() -> serde_json::Value {
        json!({"output": [{
            "type": "message",
            "content": [{
                "type": "output_text",
                "text": "grok-bot was announced in August 2026.",
                "annotations": [
                    {"url": "https://x.ai/news/grok-bot", "title": "Introducing Grok Bot"},
                    {"url": "https://x.ai/news/grok-bot", "title": "a duplicate"},
                ],
            }],
        }]})
    }

    /// The default: a search bills a person for fetches, so nothing about one
    /// is sent and no tool is offered until someone asks for it.
    #[tokio::test]
    async fn no_search_is_offered_until_it_is_configured() {
        let body = turn_body(WebSearchConfig::default(), WireApi::ChatCompletions).await;
        assert!(body.get("search_parameters").is_none());
        assert!(
            built(WebSearchConfig::default(), WireApi::ChatCompletions)
                .expect("no search is always expressible")
                .web_search()
                .is_none()
        );
    }

    /// The failure this whole seam exists to fix: a hosted search reached the
    /// model as a bare `{"type": "web_search"}` entry with no description, and
    /// a coding agent in a repository grepped and then shelled out to `curl`
    /// rather than using it. An enabled search now changes the conversation's
    /// request in no way at all.
    #[tokio::test]
    async fn an_enabled_search_leaves_the_conversations_request_alone() {
        let chat = turn_body(
            WebSearchConfig::enabled(WebSearchMode::Live),
            WireApi::ChatCompletions,
        )
        .await;
        assert!(chat.get("search_parameters").is_none());
        assert_eq!(chat["model"], json!("grok-4.6"));
        assert_eq!(chat["stream"], json!(true));

        let responses = turn_body(
            WebSearchConfig::enabled(WebSearchMode::Live),
            WireApi::Responses,
        )
        .await;
        assert!(responses.get("search_parameters").is_none());
        // The harness's own tools and nothing beside them.
        assert!(
            !responses["tools"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|tool| tool["type"] == json!("web_search")),
            "the conversation's tool list must not carry the hosted entry"
        );
    }

    /// What the model calls is a described tool, and what answers it is a
    /// request of its own carrying the search and nothing else.
    #[tokio::test]
    async fn a_search_runs_as_a_call_of_its_own_on_responses() {
        let (results, body) = search_body(
            WebSearchConfig::enabled(WebSearchMode::Live),
            WireApi::Responses,
            responses_reply(),
            "xAI grok-bot announcement",
            &[],
        )
        .await;

        assert_eq!(body["input"], json!("xAI grok-bot announcement"));
        assert_eq!(body["tools"], json!([{"type": "web_search"}]));
        assert_eq!(body["stream"], json!(false));
        // Never stored at the vendor: the rollout log is the only history.
        assert_eq!(body["store"], json!(false));

        let results = results.expect("a search");
        assert!(results.summary.contains("August 2026"));
        // One source, not two: the same URL cited twice is one place to check.
        assert_eq!(results.citations.len(), 1);
        assert_eq!(results.citations[0].url, "https://x.ai/news/grok-bot");
        assert_eq!(
            results.citations[0].title.as_deref(),
            Some("Introducing Grok Bot")
        );
    }

    /// The other wire keeps the same capability in another place and reports
    /// its sources in another shape, so both halves are read per-wire.
    #[tokio::test]
    async fn a_search_runs_as_a_call_of_its_own_on_chat_completions() {
        let (results, body) = search_body(
            WebSearchConfig::enabled(WebSearchMode::Live),
            WireApi::ChatCompletions,
            json!({
                "choices": [{"message": {"content": "grok-bot, August 2026."}}],
                "citations": ["https://x.ai/news/grok-bot"],
            }),
            "xAI grok-bot announcement",
            &[],
        )
        .await;

        assert_eq!(body["search_parameters"]["mode"], json!("auto"));
        assert_eq!(body["search_parameters"]["return_citations"], json!(true));
        assert_eq!(body["search_parameters"]["max_search_results"], json!(15));
        assert_eq!(body["stream"], json!(false));
        assert_eq!(
            body["messages"],
            json!([{"role": "user", "content": "xAI grok-bot announcement"}])
        );

        let results = results.expect("a search");
        assert!(results.summary.contains("August 2026"));
        assert_eq!(results.citations[0].url, "https://x.ai/news/grok-bot");
    }

    /// A deployment's allowlist is authoritative and the model's own list
    /// narrows it. A model that could widen it by naming a domain the config
    /// left out would make the setting advisory.
    #[tokio::test]
    async fn the_model_may_narrow_the_configured_domains_and_never_widen_them() {
        let config = WebSearchConfig {
            allowed_domains: vec!["docs.rs".to_string(), "rust-lang.org".to_string()],
            ..WebSearchConfig::enabled(WebSearchMode::Live)
        };

        // Nothing asked of it: the configured list governs.
        let (_, body) = search_body(
            config.clone(),
            WireApi::Responses,
            responses_reply(),
            "lifetimes",
            &[],
        )
        .await;
        assert_eq!(
            body["tools"][0]["filters"]["allowed_domains"],
            json!(["docs.rs", "rust-lang.org"])
        );

        // A narrower request is honored; a domain outside the policy is not.
        let (_, body) = search_body(
            config.clone(),
            WireApi::Responses,
            responses_reply(),
            "lifetimes",
            &["docs.rs".to_string(), "example.com".to_string()],
        )
        .await;
        assert_eq!(
            body["tools"][0]["filters"]["allowed_domains"],
            json!(["docs.rs"])
        );

        // A request for nothing but excluded domains is refused before
        // anything is sent: on this wire an empty list means the whole web.
        let server = MockServer::start().await;
        let provider = GrokProvider::new(
            Arc::new(StubAuth::default()),
            Endpoint {
                base_url: format!("{}/v1", server.uri()),
                default_model: Some("grok-4.6".to_string()),
                ..endpoint(config, WireApi::ChatCompletions)
            },
            None,
        )
        .expect("this configuration is expressible at xAI");
        let results = provider
            .web_search()
            .expect("a configured search")
            .search("lifetimes", &["example.com".to_string()])
            .await;
        assert!(matches!(results, Err(ProviderError::InvalidRequest(_))));
        assert!(
            server
                .received_requests()
                .await
                .expect("recorded")
                .is_empty(),
            "a domain the deployment excluded must not be searched"
        );
    }

    /// A model with no domains of its own still gets the deployment's locale
    /// and result count, since those are the endpoint's terms rather than the
    /// call's.
    #[tokio::test]
    async fn the_endpoints_terms_travel_with_every_search() {
        let (_, body) = search_body(
            WebSearchConfig {
                context_size: WebSearchContextSize::Low,
                user_location: Some(WebSearchLocation {
                    country: Some("JP".to_string()),
                    city: Some("Kyoto".to_string()),
                    ..WebSearchLocation::default()
                }),
                ..WebSearchConfig::enabled(WebSearchMode::Live)
            },
            WireApi::ChatCompletions,
            json!({"choices": [{"message": {"content": "x"}}], "citations": []}),
            "ramen",
            &[],
        )
        .await;
        assert_eq!(body["search_parameters"]["max_search_results"], json!(5));
        assert_eq!(
            body["search_parameters"]["sources"],
            json!([{"type": "web", "country": "JP"}])
        );
    }

    /// A model that names the search model pays for that one rather than for
    /// the conversation's.
    #[tokio::test]
    async fn a_named_search_model_runs_the_search() {
        let (_, body) = search_body(
            WebSearchConfig {
                model: Some("grok-4-fast".to_string()),
                ..WebSearchConfig::enabled(WebSearchMode::Live)
            },
            WireApi::Responses,
            responses_reply(),
            "anything",
            &[],
        )
        .await;
        assert_eq!(body["model"], json!("grok-4-fast"));
    }

    /// A search that found nothing is reported as a failure rather than as a
    /// successful search of nothing: a model handed an empty result under the
    /// heading "here is what the web says" writes as though it had one.
    #[tokio::test]
    async fn an_empty_result_is_an_error_rather_than_an_empty_answer() {
        let (results, _) = search_body(
            WebSearchConfig::enabled(WebSearchMode::Live),
            WireApi::Responses,
            json!({"output": []}),
            "anything",
            &[],
        )
        .await;
        let error = results.expect_err("an empty search is an error");
        assert!(
            error.to_string().contains("no results"),
            "{error} should say the search came back empty"
        );
    }

    /// xAI's search is always a live fetch. A deployment that wrote down it may
    /// not make those must not be handed them because this vendor has no
    /// narrower tier — the construction fails and names why.
    #[test]
    fn an_access_level_xai_cannot_enforce_is_refused_rather_than_approximated() {
        for mode in [WebSearchMode::Cached, WebSearchMode::Indexed] {
            for wire_api in [WireApi::ChatCompletions, WireApi::Responses] {
                let error = built(WebSearchConfig::enabled(mode), wire_api)
                    .err()
                    .expect("a mode xAI cannot honor is an error");
                assert!(
                    matches!(error, ProviderError::InvalidRequest(_)),
                    "{error} should be an invalid request"
                );
                assert!(
                    error.to_string().contains(mode.as_str()),
                    "{error} should name the mode that was refused"
                );
            }
        }
    }

    /// Disabled is disabled whatever else the config carries: a mode that is
    /// off cannot be talked back on by a domain list beside it.
    #[test]
    fn a_disabled_search_stays_off_and_still_builds() {
        let provider = built(
            WebSearchConfig {
                context_size: WebSearchContextSize::High,
                ..WebSearchConfig::default()
            },
            WireApi::ChatCompletions,
        )
        .expect("no search is always expressible");
        assert!(provider.search.is_none());
    }
}
