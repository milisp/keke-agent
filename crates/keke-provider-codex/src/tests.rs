use std::sync::Arc;

use keke_auth_api::AuthError;
use keke_auth_api::AuthFuture;
use keke_auth_api::AuthHeaders;
use keke_auth_api::AuthProvider;
use keke_auth_api::CredentialSnapshot;
use keke_auth_api::LoginUi;
use keke_provider_api::ModelProvider;
use serde_json::json;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

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
