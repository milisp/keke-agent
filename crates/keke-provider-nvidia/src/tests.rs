use std::sync::Arc;

use futures::StreamExt;
use keke_auth_api::AuthError;
use keke_auth_api::AuthFuture;
use keke_auth_api::AuthHeaders;
use keke_auth_api::AuthProvider;
use keke_auth_api::CredentialSnapshot;
use keke_auth_api::LoginUi;
use keke_protocol::Message;
use keke_protocol::StopReason;
use keke_provider_api::ModelProvider;
use keke_provider_api::ModelRequest;
use keke_provider_api::StreamChunk;
use keke_provider_api::WireApi;
use serde_json::json;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

use super::NvidiaProvider;

struct StubAuth;

impl AuthProvider for StubAuth {
    fn id(&self) -> &'static str {
        "nvidia"
    }

    fn snapshot(&self) -> CredentialSnapshot {
        CredentialSnapshot {
            auth_id: "nvidia".to_string(),
            source: "test".to_string(),
            ..CredentialSnapshot::default()
        }
    }

    fn headers(&self) -> AuthFuture<'_, Result<AuthHeaders, AuthError>> {
        Box::pin(async { Ok(AuthHeaders::bearer("nim-key")) })
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

#[test]
fn provider_info_names_its_route_credentials_and_endpoint() {
    let provider = NvidiaProvider::new(Arc::new(StubAuth), None);
    let info = provider.info();

    assert_eq!(info.route, "nvidia");
    assert_eq!(info.display_name, "NVIDIA NIM");
    assert_eq!(info.auth_id.as_deref(), Some("nvidia"));
    assert_eq!(info.env_key.as_deref(), Some("NVIDIA_API_KEY"));
    assert_eq!(info.base_url, "https://integrate.api.nvidia.com/v1");
    assert_eq!(info.wire_api, WireApi::ChatCompletions);
    assert_eq!(super::DEFAULT_MODEL, "nvidia/nemotron-3-ultra-550b-a55b");
}

#[test]
fn the_responses_endpoint_can_be_selected_and_anthropic_cannot() {
    let responses =
        NvidiaProvider::with_wire_api(Arc::new(StubAuth), None, WireApi::Responses).expect("built");
    assert_eq!(responses.info().wire_api, WireApi::Responses);

    assert!(NvidiaProvider::with_wire_api(Arc::new(StubAuth), None, WireApi::Messages).is_err());
    assert!(NvidiaProvider::with_wire_api(Arc::new(StubAuth), None, WireApi::Custom).is_err());
}

#[tokio::test]
async fn a_reply_streams_through_the_wire_client() {
    let server = MockServer::start().await;
    let body = [
        json!({"choices":[{"delta":{"content":"nim"}}]}).to_string(),
        json!({"choices":[{"delta":{},"finish_reason":"stop"}]}).to_string(),
        "[DONE]".to_string(),
    ]
    .iter()
    .map(|frame| format!("data: {frame}\n\n"))
    .collect::<String>();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(&server)
        .await;

    let provider = NvidiaProvider::new(Arc::new(StubAuth), Some(format!("{}/v1", server.uri())));
    let chunks: Vec<_> = provider
        .stream(ModelRequest {
            model: super::DEFAULT_MODEL.to_string(),
            messages: vec![Message::user("hi")],
            ..ModelRequest::default()
        })
        .await
        .expect("stream starts")
        .map(|chunk| chunk.expect("no stream error"))
        .collect()
        .await;

    assert_eq!(
        chunks,
        vec![
            StreamChunk::TextDelta("nim".to_string()),
            StreamChunk::Done(StopReason::EndTurn),
        ]
    );
}
