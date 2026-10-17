use std::sync::Arc;

use keke_auth_api::AuthError;
use keke_auth_api::AuthFuture;
use keke_auth_api::AuthHeaders;
use keke_auth_api::AuthProvider;
use keke_auth_api::CredentialSnapshot;
use keke_auth_api::LoginUi;
use keke_config_types::WebSearchConfig;
use keke_config_types::WebSearchContextSize;
use keke_config_types::WebSearchMode;
use keke_provider_api::ProviderError;
use keke_provider_api::WebSearchBackend;
use keke_provider_api::WebSearchResults;
use keke_wire::WireClient;
use serde_json::Value;
use serde_json::json;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

use super::OpenRouterWebSearch;

struct StubAuth;

impl AuthProvider for StubAuth {
    fn id(&self) -> &'static str {
        "openrouter"
    }

    fn snapshot(&self) -> CredentialSnapshot {
        CredentialSnapshot {
            auth_id: "openrouter".to_string(),
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

fn wire(base_url: String) -> WireClient {
    WireClient::new(base_url, Arc::new(StubAuth))
}

fn live() -> WebSearchConfig {
    WebSearchConfig::enabled(WebSearchMode::Live)
}

/// One search against a canned reply, and the request bodies it sent.
async fn search(
    config: WebSearchConfig,
    reply: Value,
    domains: &[String],
) -> (Result<WebSearchResults, ProviderError>, Vec<Value>) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(reply.to_string()))
        .mount(&server)
        .await;
    let backend = OpenRouterWebSearch::new(
        wire(format!("{}/api/v1", server.uri())),
        config,
        Some("anthropic/claude-sonnet-5.5".to_string()),
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
    json!({"choices": [{"message": {
        "role": "assistant",
        "content": "The 2027 edition ships in Rust 1.110.",
        "annotations": [
            {"type": "url_citation", "url_citation": {
                "url": "https://blog.rust-lang.org/2027", "title": "Rust 2027",
                "content": "...", "start_index": 0, "end_index": 10}},
            {"type": "url_citation", "url_citation": {
                "url": "https://blog.rust-lang.org/2027", "title": "a duplicate"}},
        ],
    }}]})
}

/// Off by default: a search bills a person, so nothing is offered until
/// someone asks for it.
#[test]
fn no_search_is_offered_until_it_is_configured() {
    let backend = OpenRouterWebSearch::new(
        wire("http://unused".to_string()),
        WebSearchConfig::default(),
        None,
    )
    .expect("no search is always expressible");
    assert!(backend.is_none());
}

/// Refused at launch: a tier OpenRouter cannot express, and a search with no
/// model to run it, rather than at the first search.
#[test]
fn an_unrunnable_search_is_refused_at_construction() {
    for mode in [WebSearchMode::Cached, WebSearchMode::Indexed] {
        let refused = OpenRouterWebSearch::new(
            wire("http://unused".to_string()),
            WebSearchConfig::enabled(mode),
            Some("openai/gpt-5.2".to_string()),
        );
        assert!(matches!(refused, Err(ProviderError::InvalidRequest(_))));
    }
    let no_model = OpenRouterWebSearch::new(wire("http://unused".to_string()), live(), None);
    assert!(matches!(no_model, Err(ProviderError::InvalidRequest(_))));
}

/// The search is a request of its own carrying the server tool and nothing
/// else, and its answer comes back with the sources it cited.
#[tokio::test]
async fn a_search_runs_as_a_call_of_its_own() {
    let mut config = live();
    config.context_size = WebSearchContextSize::Low;
    let (results, bodies) = search(config, answer(), &[]).await;

    let body = &bodies[0];
    assert_eq!(body["model"], json!("anthropic/claude-sonnet-5.5"));
    assert_eq!(
        body["tools"],
        json!([{"type": "openrouter:web_search",
                "parameters": {"max_uses": 1, "search_context_size": "low"}}])
    );
    assert!(body.get("tool_choice").is_none());

    let results = results.expect("a search");
    assert_eq!(results.summary, "The 2027 edition ships in Rust 1.110.");
    assert_eq!(results.citations.len(), 1, "deduplicated");
    assert_eq!(results.citations[0].title.as_deref(), Some("Rust 2027"));
}

/// The model's domains narrow the deployment's and never widen them.
#[tokio::test]
async fn a_model_cannot_search_a_domain_the_deployment_excluded() {
    let mut config = live();
    config.allowed_domains = vec!["docs.rs".to_string(), "rust-lang.org".to_string()];

    let (_, bodies) = search(config.clone(), answer(), &["docs.rs".to_string()]).await;
    assert_eq!(
        bodies[0]["tools"][0]["parameters"]["allowed_domains"],
        json!(["docs.rs"])
    );

    let (results, bodies) = search(config, answer(), &["evil.example".to_string()]).await;
    assert!(matches!(results, Err(ProviderError::InvalidRequest(_))));
    assert!(bodies.is_empty(), "nothing is sent unconfined");
}

/// A reply with neither an answer nor a source is a failure, not results.
#[tokio::test]
async fn an_empty_search_is_reported_not_answered() {
    let empty = json!({"choices": [{"message": {"role": "assistant", "content": ""}}]});
    let (results, _) = search(live(), empty, &[]).await;
    assert!(matches!(results, Err(ProviderError::Protocol(_))));
}
