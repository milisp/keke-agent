//! ChatGPT's authorization-code login, over the shared loopback redirect.
//!
//! What is here is what the issuer decides. The redirect server itself, PKCE,
//! and the state check are `keke-oauth`'s — the same code every issuer needs.

use keke_auth_api::AuthError;
use keke_auth_api::LoginUi;
use keke_oauth::Loopback;
use keke_oauth::Pkce;
use keke_oauth::random_token;
use reqwest::Client;
use url::Url;

use keke_credentials::AuthTokens;

use crate::CodexAuthConfig;
use crate::endpoint::exchange;

use crate::ported::codex::authorize;
use crate::ported::codex::authorize::CALLBACK_PATH;

/// Claim the one loopback port this client's redirect URI is registered at.
///
/// Not port 0: see [`crate::config::DEFAULT_CALLBACK_PORT`]. A port already in
/// use means another login is in flight, and the caller falls back to the
/// device-code flow rather than opening a browser at an address the issuer
/// will refuse.
pub(crate) async fn bind(port: u16) -> std::io::Result<Loopback> {
    Loopback::bind(port, CALLBACK_PATH).await
}

pub(crate) async fn run(
    http: &Client,
    config: &CodexAuthConfig,
    ui: &dyn LoginUi,
    loopback: Loopback,
) -> Result<AuthTokens, AuthError> {
    // Upstream's registration names `localhost`, which is not what
    // `Loopback::redirect_uri` builds — an issuer that compares the string
    // would refuse it — so the URI is the ported builder's and only the port
    // comes from the listener.
    let redirect_uri = authorize::redirect_uri(loopback.port()?);

    let pkce = Pkce::generate();
    let state = random_token(16);
    let url = authorize_url(config, &redirect_uri, &pkce, &state)?;

    ui.open_browser(url.as_str());
    ui.notice("waiting for the browser to complete sign-in");

    let code = loopback.await_code(&state, config.login_timeout).await?;

    let tokens = exchange(
        http,
        &config.token_endpoint,
        &[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
            ("client_id", config.client_id.as_str()),
            ("code_verifier", pkce.verifier.as_str()),
        ],
    )
    .await?;

    Ok(tokens.into_tokens(None, None, Some(config.issuer.clone())))
}

/// Delegates to the ported upstream builder — see
/// [`crate::ported::codex::authorize`] for why this flow's shape is not ours
/// to derive.
fn authorize_url(
    config: &CodexAuthConfig,
    redirect_uri: &str,
    pkce: &Pkce,
    state: &str,
) -> Result<Url, AuthError> {
    let url = authorize::build_authorize_url(
        &config.authorize_endpoint,
        &config.client_id,
        redirect_uri,
        &config.scope_param(),
        &pkce.challenge,
        state,
        &config.originator,
    );
    Url::parse(&url)
        .map_err(|err| AuthError::Other(format!("authorize endpoint is not a URL: {err}")))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn the_authorize_url_carries_the_s256_challenge() {
        let config = CodexAuthConfig::new("https://issuer.test", "client-1");
        let pkce = Pkce::generate();
        let url = authorize_url(&config, "http://127.0.0.1:1234/callback", &pkce, "st").unwrap();
        let params: std::collections::BTreeMap<_, _> = url.query_pairs().collect();
        assert_eq!(params["response_type"], "code");
        assert_eq!(params["code_challenge_method"], "S256");
        assert_eq!(params["code_challenge"], pkce.challenge.as_str());
        assert_eq!(params["redirect_uri"], "http://127.0.0.1:1234/callback");
        assert!(!params.contains_key("code_verifier"));
    }

    #[tokio::test]
    async fn the_loopback_flow_exchanges_the_redirected_code() {
        use keke_auth_api::AuthProvider as _;
        use keke_credentials::MemoryStore;
        use std::sync::Arc;
        use wiremock::Mock;
        use wiremock::MockServer;
        use wiremock::ResponseTemplate;
        use wiremock::matchers::method;
        use wiremock::matchers::path;

        use crate::CodexAuth;
        use crate::test_support::Home;
        use crate::test_support::RecordingUi;
        use crate::test_support::stored_tokens;

        let server = MockServer::start().await;
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
        let auth = Arc::new(
            CodexAuth::new(
                Arc::new(MemoryStore::new()),
                home.auth_files(),
                CodexAuthConfig::new(server.uri(), "client-1"),
            )
            .with_importer(home.importer()),
        );
        let ui = RecordingUi::new();

        let flow = tokio::spawn({
            let auth = auth.clone();
            let ui = ui.clone();
            async move { auth.login(ui).await }
        });

        // The authorize URL is the only place the redirect port and the state
        // this login issued are published, which is the point of reading it
        // back rather than predicting it.
        let authorize = loop {
            if let Some(url) = ui.browser_urls().first() {
                break Url::parse(url).unwrap();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        let params: std::collections::BTreeMap<_, _> = authorize.query_pairs().collect();
        let redirect = Url::parse(&params["redirect_uri"]).unwrap();

        let callback = format!("{redirect}?code=auth-code-1&state={}", params["state"]);
        reqwest::get(&callback).await.unwrap();

        flow.await.unwrap().unwrap();

        assert_eq!(
            stored_tokens(&auth).expect("tokens").access_token,
            "access-1"
        );
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
