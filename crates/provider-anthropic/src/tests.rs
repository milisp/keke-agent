use std::sync::Arc;

use keke_auth_api::AuthError;
use keke_auth_api::AuthFuture;
use keke_auth_api::AuthHeaders;
use keke_auth_api::AuthProvider;
use keke_auth_api::CredentialSnapshot;
use keke_auth_api::LoginUi;
use keke_config_types::WebSearchConfig;
use keke_config_types::WebSearchContextSize;
use keke_config_types::WebSearchLocation;
use keke_config_types::WebSearchMode;
use keke_provider_api::ProviderError;
use keke_provider_api::WebSearchBackend;
use keke_provider_api::WebSearchResults;
use serde_json::Value;
use serde_json::json;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;

use super::AnthropicWebSearch;

struct StubAuth;

impl AuthProvider for StubAuth {
    fn id(&self) -> &'static str {
        "anthropic"
    }

    fn snapshot(&self) -> CredentialSnapshot {
        CredentialSnapshot {
            auth_id: "anthropic".to_string(),
            source: "test".to_string(),
            ..CredentialSnapshot::default()
        }
    }

    fn headers(&self) -> AuthFuture<'_, Result<AuthHeaders, AuthError>> {
        Box::pin(async { Ok(AuthHeaders::bearer("key")) })
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

fn live() -> WebSearchConfig {
    WebSearchConfig::enabled(WebSearchMode::Live)
}

fn built(
    base_url: String,
    config: WebSearchConfig,
    default_model: Option<&str>,
) -> Result<Option<AnthropicWebSearch>, ProviderError> {
    AnthropicWebSearch::new(
        base_url,
        Arc::new(StubAuth),
        config,
        default_model.map(str::to_string),
    )
}

/// One search against canned replies, and the request bodies it sent.
async fn search(
    config: WebSearchConfig,
    replies: &[Value],
    domains: &[String],
) -> (Result<WebSearchResults, ProviderError>, Vec<Value>) {
    let server = MockServer::start().await;
    for reply in replies {
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("anthropic-version", "2023-06-01"))
            .respond_with(ResponseTemplate::new(200).set_body_string(reply.to_string()))
            .up_to_n_times(1)
            .mount(&server)
            .await;
    }
    let backend = built(
        format!("{}/v1", server.uri()),
        config,
        Some("claude-opus-5-5"),
    )
    .expect("expressible")
    .expect("enabled");
    let results = backend.search("rust 2027 edition", domains).await;
    let bodies = server
        .received_requests()
        .await
        .expect("recorded")
        .iter()
        .map(|request| serde_json::from_slice(&request.body).expect("a JSON body"))
        .collect();
    (results, bodies)
}

fn answer() -> Value {
    json!({
        "stop_reason": "end_turn",
        "content": [
            {"type": "text", "text": "I'll search for that."},
            {"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search",
             "input": {"query": "rust 2027 edition"}},
            {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_1", "content": [
                {"type": "web_search_result", "url": "https://blog.rust-lang.org/2027",
                 "title": "Rust 2027", "encrypted_content": "x"},
                {"type": "web_search_result", "url": "https://example.com/uncited",
                 "title": "Uncited", "encrypted_content": "y"},
            ]},
            {"type": "text", "text": "The 2027 edition "},
            {"type": "text", "text": "ships in Rust 1.110.", "citations": [
                {"type": "web_search_result_location", "url": "https://blog.rust-lang.org/2027",
                 "title": "Rust 2027", "cited_text": "..."},
                {"type": "web_search_result_location", "url": "https://blog.rust-lang.org/2027",
                 "title": "Rust 2027", "cited_text": "..."},
            ]},
        ],
    })
}

/// Off by default: a search bills a person, so nothing is offered until
/// someone asks for it.
#[test]
fn no_search_is_offered_until_it_is_configured() {
    let backend = built(
        "http://unused".to_string(),
        WebSearchConfig::default(),
        None,
    )
    .expect("no search is always expressible");
    assert!(backend.is_none());
}

/// A tier this vendor has no counterpart for is refused at construction,
/// never approximated with live access the deployment withheld.
#[test]
fn an_access_tier_anthropic_cannot_express_is_refused() {
    for mode in [WebSearchMode::Cached, WebSearchMode::Indexed] {
        let refused = built(
            "http://unused".to_string(),
            WebSearchConfig::enabled(mode),
            None,
        );
        assert!(matches!(refused, Err(ProviderError::InvalidRequest(_))));
    }
}

/// The search is a request of its own carrying the server tool and nothing
/// else, and its answer is the text after the results with the sources it
/// cited.
#[tokio::test]
async fn a_search_runs_as_a_call_of_its_own() {
    let mut config = live();
    config.context_size = WebSearchContextSize::High;
    config.user_location = Some(WebSearchLocation {
        country: Some("US".to_string()),
        ..WebSearchLocation::default()
    });
    let (results, bodies) = search(config, &[answer()], &[]).await;

    let body = &bodies[0];
    assert_eq!(body["model"], json!("claude-opus-5-5"));
    assert_eq!(
        body["tools"],
        json!([{
            "type": "web_search_20250305",
            "name": "web_search",
            "max_uses": 5,
            "user_location": {"type": "approximate", "country": "US"},
        }])
    );
    // Current models refuse a forced tool choice outright.
    assert!(body.get("tool_choice").is_none());

    let results = results.expect("a search");
    assert_eq!(results.summary, "The 2027 edition ships in Rust 1.110.");
    assert_eq!(results.citations.len(), 1, "cited sources, deduplicated");
    assert_eq!(results.citations[0].url, "https://blog.rust-lang.org/2027");
    assert_eq!(results.citations[0].title.as_deref(), Some("Rust 2027"));
}

/// The model's domains narrow the deployment's and never widen them.
#[tokio::test]
async fn a_model_cannot_search_a_domain_the_deployment_excluded() {
    let mut config = live();
    config.allowed_domains = vec!["docs.rs".to_string(), "rust-lang.org".to_string()];

    let (_, bodies) = search(config.clone(), &[answer()], &["rust-lang.org".to_string()]).await;
    assert_eq!(
        bodies[0]["tools"][0]["allowed_domains"],
        json!(["rust-lang.org"])
    );

    let (results, bodies) = search(config, &[answer()], &["evil.example".to_string()]).await;
    assert!(matches!(results, Err(ProviderError::InvalidRequest(_))));
    assert!(bodies.is_empty(), "nothing is sent unconfined");
}

/// A paused turn is handed back and resumed, so the answer that follows is
/// the one reported.
#[tokio::test]
async fn a_paused_search_is_resumed() {
    let paused = json!({
        "stop_reason": "pause_turn",
        "content": [{"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search",
                     "input": {"query": "rust 2027 edition"}}],
    });
    let (results, bodies) = search(live(), &[paused.clone(), answer()], &[]).await;

    assert_eq!(bodies.len(), 2);
    assert_eq!(
        bodies[1]["messages"][1],
        json!({"role": "assistant", "content": paused["content"]})
    );
    assert_eq!(
        results.expect("a search").summary,
        "The 2027 edition ships in Rust 1.110."
    );
}

/// A server-tool error arrives as a successful reply; it is still a failure,
/// and the vendor's code is what the model is told.
#[tokio::test]
async fn a_failed_search_is_reported_not_answered() {
    let failed = json!({
        "stop_reason": "end_turn",
        "content": [{"type": "web_search_tool_result", "tool_use_id": "srvtoolu_1",
                     "content": {"type": "web_search_tool_result_error",
                                 "error_code": "too_many_requests"}}],
    });
    let (results, _) = search(live(), &[failed], &[]).await;
    match results {
        Err(ProviderError::Protocol(message)) => assert!(message.contains("too_many_requests")),
        other => panic!("expected the vendor's error, got {other:?}"),
    }
}
