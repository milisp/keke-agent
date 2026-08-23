//! Authorization code + PKCE over a loopback redirect (RFC 8252 §7.3).
//!
//! The port is bound *before* the authorize URL is built, because the port
//! number is part of the `redirect_uri` the issuer will check; binding
//! afterwards would leave a window where the URL names a port we do not own.

use std::time::Duration;

use keke_auth_api::AuthError;
use keke_auth_api::LoginUi;
use reqwest::Client;
use tokio::io::AsyncReadExt as _;
use tokio::io::AsyncWriteExt as _;
use tokio::net::TcpListener;
use url::Url;

use keke_credentials::AuthTokens;

use crate::CodexAuthConfig;
use crate::endpoint::exchange;
use crate::pkce::Pkce;
use crate::pkce::random_token;

use crate::ported::codex::authorize;
use crate::ported::codex::authorize::CALLBACK_PATH;
/// A request line plus headers; anything larger is not a browser redirect.
const MAX_REQUEST_BYTES: usize = 8 * 1024;

const DONE_PAGE: &str = "<!doctype html><meta charset=utf-8><title>Signed in</title>\
<p>Signed in. You can close this tab and return to the terminal.";

/// Claim the one loopback port this client's redirect URI is registered at.
///
/// Not port 0: see [`crate::config::DEFAULT_CALLBACK_PORT`]. A port already in
/// use means another login is in flight, and the caller falls back to the
/// device-code flow rather than opening a browser at an address the issuer
/// will refuse.
pub(crate) async fn bind(port: u16) -> std::io::Result<TcpListener> {
    TcpListener::bind(("127.0.0.1", port)).await
}

pub(crate) async fn run(
    http: &Client,
    config: &CodexAuthConfig,
    ui: &dyn LoginUi,
    listener: TcpListener,
) -> Result<AuthTokens, AuthError> {
    let port = listener
        .local_addr()
        .map_err(|err| AuthError::Other(format!("loopback address unavailable: {err}")))?
        .port();
    let redirect_uri = authorize::redirect_uri(port);

    let pkce = Pkce::generate();
    let state = random_token(16);
    let url = authorize_url(config, &redirect_uri, &pkce, &state)?;

    ui.open_browser(url.as_str());
    ui.notice("waiting for the browser to complete sign-in");

    let code = tokio::time::timeout(config.login_timeout, await_callback(listener, &state))
        .await
        .map_err(|_| AuthError::Cancelled)??;

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

    Ok(tokens.into_tokens(None, None))
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

/// Serve exactly one callback and return its `code`.
async fn await_callback(listener: TcpListener, state: &str) -> Result<String, AuthError> {
    loop {
        let (mut socket, _) = listener
            .accept()
            .await
            .map_err(|err| AuthError::Other(format!("loopback accept failed: {err}")))?;

        let Some(target) = read_request_target(&mut socket).await else {
            continue;
        };
        // Browsers ask for /favicon.ico on the same connection budget; only the
        // callback ends the wait.
        let Ok(url) = Url::parse(&format!("http://127.0.0.1{target}")) else {
            continue;
        };
        if url.path() != CALLBACK_PATH {
            let _ = respond(&mut socket, "404 Not Found", "Not found.").await;
            continue;
        }

        let outcome = classify(&url, state);
        let _ = match &outcome {
            Ok(_) => respond(&mut socket, "200 OK", DONE_PAGE).await,
            Err(err) => respond(&mut socket, "400 Bad Request", &err.to_string()).await,
        };
        return outcome;
    }
}

fn classify(url: &Url, state: &str) -> Result<String, AuthError> {
    let mut code = None;
    let mut returned_state = None;
    let mut error = None;
    let mut description = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => returned_state = Some(value.into_owned()),
            "error" => error = Some(value.into_owned()),
            "error_description" => description = Some(value.into_owned()),
            _ => {}
        }
    }

    if let Some(error) = error {
        let detail = description.unwrap_or_else(|| error.clone());
        return Err(match error.as_str() {
            "access_denied" => AuthError::Cancelled,
            _ => AuthError::Rejected(detail),
        });
    }
    // Anything on 127.0.0.1 can reach this port; without the state check a
    // local process could feed us its own authorization code.
    if returned_state.as_deref() != Some(state) {
        return Err(AuthError::Rejected(
            "the redirect did not carry the state this login issued".into(),
        ));
    }
    code.ok_or_else(|| AuthError::Rejected("the redirect carried no authorization code".into()))
}

/// Read the request target from the first line, bounded so a client that never
/// sends a blank line cannot hold the login open.
async fn read_request_target(socket: &mut tokio::net::TcpStream) -> Option<String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let read = tokio::time::timeout(Duration::from_secs(10), socket.read(&mut chunk))
            .await
            .ok()?
            .ok()?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.windows(4).any(|w| w == b"\r\n\r\n") || buffer.len() >= MAX_REQUEST_BYTES {
            break;
        }
    }

    let text = String::from_utf8_lossy(&buffer);
    let mut parts = text.lines().next()?.split(' ');
    let method = parts.next()?;
    let target = parts.next()?;
    (method == "GET").then(|| target.to_string())
}

async fn respond(
    socket: &mut tokio::net::TcpStream,
    status: &str,
    body: &str,
) -> std::io::Result<()> {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(response.as_bytes()).await?;
    socket.shutdown().await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn callback(query: &str) -> Url {
        Url::parse(&format!("http://127.0.0.1/callback?{query}")).unwrap()
    }

    #[test]
    fn a_redirect_with_a_foreign_state_is_rejected() {
        let err = classify(&callback("code=abc&state=someone-else"), "ours").unwrap_err();
        assert!(matches!(err, AuthError::Rejected(_)), "got {err:?}");
    }

    #[test]
    fn a_denied_redirect_reads_as_cancellation() {
        let err = classify(&callback("error=access_denied&state=ours"), "ours").unwrap_err();
        assert!(matches!(err, AuthError::Cancelled), "got {err:?}");
    }

    #[test]
    fn a_matching_redirect_yields_the_code() {
        assert_eq!(
            classify(&callback("code=abc&state=ours"), "ours").unwrap(),
            "abc"
        );
    }

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
