//! The RFC 8628 device grant against ChatGPT's endpoints. The grant itself is
//! `keke_oauth::device`'s.

use keke_auth_api::AuthError;
use keke_auth_api::LoginUi;
use keke_credentials::AuthTokens;
use keke_oauth::device::Delay;
use keke_oauth::device::DeviceFlow;
use reqwest::Client;

use crate::CodexAuthConfig;

pub(crate) async fn run(
    http: &Client,
    config: &CodexAuthConfig,
    ui: &dyn LoginUi,
    delay: &dyn Delay,
) -> Result<AuthTokens, AuthError> {
    let scope = config.scope_param();
    let flow = DeviceFlow {
        device_authorization_endpoint: &config.device_authorization_endpoint,
        token_endpoint: &config.token_endpoint,
        client_id: &config.client_id,
        scope: &scope,
        slow_down_increment: config.slow_down_increment,
    };
    let response = keke_oauth::device::run(http, &flow, ui, delay).await?;
    Ok(crate::tokens::into_tokens(
        response,
        None,
        None,
        Some(config.issuer.clone()),
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use keke_auth_api::AuthProvider as _;
    use keke_credentials::MemoryStore;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    use super::*;
    use crate::CodexAuth;
    use crate::test_support::Home;
    use crate::test_support::RecordingDelay;
    use crate::test_support::RecordingUi;
    use crate::test_support::stored_tokens;

    #[tokio::test]
    async fn a_device_code_login_stores_the_token() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/device/code"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "device_code": "dev-code-1",
                "user_code": "ABCD-1234",
                "verification_uri": "https://chatgpt.com/device",
                "verification_uri_complete": "https://chatgpt.com/device?user_code=ABCD-1234",
                "expires_in": 600,
                "interval": 2,
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "access-1",
                "refresh_token": "refresh-1",
                "expires_in": 3600,
            })))
            .mount(&server)
            .await;

        let home = Home::new();
        let ui = RecordingUi::new();
        let delay = RecordingDelay::new();
        let auth = CodexAuth::new(
            Arc::new(MemoryStore::new()),
            home.auth_files(),
            CodexAuthConfig::new(server.uri(), "client-1").device_code_only(true),
        )
        .with_importer(home.importer())
        .with_delay(delay.clone());

        auth.login(ui.clone()).await.unwrap();

        assert_eq!(
            ui.device_codes(),
            vec![(
                "ABCD-1234".to_string(),
                "https://chatgpt.com/device".to_string()
            )]
        );
        assert_eq!(
            ui.browser_urls(),
            vec!["https://chatgpt.com/device?user_code=ABCD-1234".to_string()]
        );

        let stored = stored_tokens(&auth).expect("tokens");
        assert_eq!(stored.access_token, "access-1");
        assert_eq!(stored.refresh_token.as_deref(), Some("refresh-1"));
        assert_eq!(
            auth.auth_files
                .load(&auth.config().vendor)
                .expect("load")
                .expect("present")
                .auth_mode
                .as_str(),
            "chatgpt"
        );
    }
}
