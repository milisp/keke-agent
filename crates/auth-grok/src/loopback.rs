//! xAI's authorization-code login, over the shared loopback redirect.
//!
//! What is here is what the issuer decides: the authorize URL's parameters and
//! the token request's body. The redirect server itself, PKCE, and the state
//! check are `keke-oauth`'s — the same code every issuer needs.

use keke_auth_api::AuthError;
use keke_auth_api::LoginUi;
use keke_oauth::Loopback;
use keke_oauth::Pkce;
use keke_oauth::random_token;
use reqwest::Client;
use url::Url;

use keke_credentials::AuthTokens;

use crate::GrokAuthConfig;
use crate::endpoint::exchange;

const CALLBACK_PATH: &str = "/callback";

/// Claim a loopback port, or report why this machine cannot host the redirect.
pub(crate) async fn bind() -> std::io::Result<Loopback> {
    Loopback::bind(0, CALLBACK_PATH).await
}

pub(crate) async fn run(
    http: &Client,
    discovery: &crate::discovery::Cache,
    config: &GrokAuthConfig,
    ui: &dyn LoginUi,
    loopback: Loopback,
) -> Result<AuthTokens, AuthError> {
    let redirect_uri = loopback.redirect_uri()?;

    let pkce = Pkce::generate();
    let state = random_token(16);
    let url = authorize_url(
        &crate::discovery::authorize_endpoint(http, discovery, config).await,
        config,
        &redirect_uri,
        &pkce,
        &state,
    )?;

    ui.open_browser(url.as_str());
    ui.notice("waiting for the browser to complete sign-in");

    let code = loopback.await_code(&state, config.login_timeout).await?;

    let tokens = exchange(
        http,
        &crate::discovery::token_endpoint(http, discovery, config, None).await,
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

fn authorize_url(
    authorize_endpoint: &str,
    config: &GrokAuthConfig,
    redirect_uri: &str,
    pkce: &Pkce,
    state: &str,
) -> Result<Url, AuthError> {
    let mut url = Url::parse(authorize_endpoint)
        .map_err(|err| AuthError::Other(format!("authorize endpoint is not a URL: {err}")))?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &config.client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", &config.scope_param())
        .append_pair("state", state)
        .append_pair("code_challenge", &pkce.challenge)
        .append_pair("code_challenge_method", "S256");
    Ok(url)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn the_authorize_url_carries_the_s256_challenge() {
        let config = GrokAuthConfig::new("https://issuer.test", "client-1");
        let pkce = Pkce::generate();
        let url = authorize_url(
            &config.authorize_endpoint,
            &config,
            "http://127.0.0.1:1234/callback",
            &pkce,
            "st",
        )
        .unwrap();
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

        use crate::GrokAuth;
        use crate::test_support::Home;
        use crate::test_support::RecordingUi;
        use crate::test_support::stored_tokens;

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth2/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "access-1",
                "refresh_token": "refresh-1",
                "expires_in": 3600,
            })))
            .mount(&server)
            .await;

        let home = Home::new();
        let auth = Arc::new(
            GrokAuth::new(
                Arc::new(MemoryStore::new()),
                home.auth_files(),
                GrokAuthConfig::new(server.uri(), "client-1"),
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
            "oidc"
        );
    }
}
