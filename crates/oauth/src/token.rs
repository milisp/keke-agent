//! The token endpoint (RFC 6749 §5), and the arithmetic around what it returns.
//!
//! What a request *says* — grant type, which parameters, form or JSON — is the
//! issuer's, so callers build the body. What comes back, and how a refusal is
//! told apart from progress, is the RFC's.

use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use keke_auth_api::AuthError;
use reqwest::Client;
use serde::Deserialize;

/// A token request runs while the credential's mutation lock is held, and that
/// lock is broken as abandoned after a minute. A request that outlived it would
/// be writing its answer into a file another process had taken over.
const TOKEN_TIMEOUT: Duration = Duration::from_secs(15);

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

/// When a credential stops working, preferring the token's own `exp` over the
/// `recorded` one: a refresh that reused a stored expiry would keep reporting
/// the original login's deadline forever.
pub fn expires_at(access_token: &str, recorded: Option<i64>) -> Option<i64> {
    crate::jwt::expiry(access_token).or(recorded)
}

/// Whether a credential expiring at `expires_at` is expired, or close enough
/// that a request made now might arrive after it is.
///
/// A credential with no stated expiry is never stale: an API key is not
/// refreshable and treating it as expired would loop.
pub fn is_stale(expires_at: Option<i64>, leeway: Duration) -> bool {
    match expires_at {
        Some(exp) => exp <= now() + leeway.as_secs() as i64,
        None => false,
    }
}

/// A successful response from the token endpoint, in any grant.
#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: Option<i64>,
}

impl TokenResponse {
    /// The absolute deadline `expires_in` names, counted from now.
    pub fn expires_at(&self) -> Option<i64> {
        self.expires_in.map(|seconds| now() + seconds)
    }
}

/// An OAuth2 error response, per RFC 6749 §5.2.
#[derive(Debug, Deserialize)]
pub struct TokenError {
    pub error: String,
    pub error_description: Option<String>,
}

impl TokenError {
    pub fn detail(&self) -> &str {
        self.error_description.as_deref().unwrap_or(&self.error)
    }
}

/// What the token endpoint said.
///
/// `Refused` is a value rather than an error because the device-code flow reads
/// `authorization_pending` and `slow_down` as *progress*; collapsing them into
/// an error type would mean parsing an error message back into control flow.
pub enum TokenOutcome {
    Granted(TokenResponse),
    Refused(TokenError),
}

pub async fn post_token(
    http: &Client,
    url: &str,
    form: &[(&str, &str)],
) -> Result<TokenOutcome, AuthError> {
    read_token_response(http.post(url).form(form)).await
}

/// The same exchange with a JSON body.
///
/// Not every issuer takes the RFC's form encoding everywhere: OpenAI's token
/// endpoint answers a form-encoded refresh with an error, so a refresh that
/// renews nothing looks exactly like a revoked credential.
pub async fn post_token_json(
    http: &Client,
    url: &str,
    body: &serde_json::Value,
) -> Result<TokenOutcome, AuthError> {
    read_token_response(http.post(url).json(body)).await
}

async fn read_token_response(request: reqwest::RequestBuilder) -> Result<TokenOutcome, AuthError> {
    let response = request
        .timeout(TOKEN_TIMEOUT)
        .send()
        .await
        .map_err(|err| AuthError::Other(format!("token endpoint unreachable: {err}")))?;

    let status = response.status();
    let body = response
        .bytes()
        .await
        .map_err(|err| AuthError::Other(format!("token endpoint response truncated: {err}")))?;

    if status.is_success() {
        return serde_json::from_slice(&body)
            .map(TokenOutcome::Granted)
            .map_err(|_| AuthError::Other("token endpoint returned an unreadable grant".into()));
    }

    // A non-OAuth2 error body (a proxy's HTML, say) still has to become
    // something the caller can match on, so it is reported by status.
    Ok(serde_json::from_slice(&body)
        .map(TokenOutcome::Refused)
        .unwrap_or_else(|_| {
            TokenOutcome::Refused(TokenError {
                error: format!("http_{}", status.as_u16()),
                error_description: None,
            })
        }))
}

/// Post to the token endpoint where any refusal is terminal.
pub async fn exchange(
    http: &Client,
    url: &str,
    form: &[(&str, &str)],
) -> Result<TokenResponse, AuthError> {
    terminal(post_token(http, url, form).await?)
}

/// [`exchange`] with a JSON body — see [`post_token_json`].
pub async fn exchange_json(
    http: &Client,
    url: &str,
    body: &serde_json::Value,
) -> Result<TokenResponse, AuthError> {
    terminal(post_token_json(http, url, body).await?)
}

fn terminal(outcome: TokenOutcome) -> Result<TokenResponse, AuthError> {
    match outcome {
        TokenOutcome::Granted(tokens) => Ok(tokens),
        TokenOutcome::Refused(err) => Err(AuthError::Rejected(err.detail().to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_credential_without_an_expiry_is_never_stale() {
        assert!(!is_stale(
            expires_at("sk-key", None),
            Duration::from_secs(60)
        ));
    }

    #[test]
    fn the_leeway_makes_a_soon_to_expire_token_stale() {
        let soon = expires_at("opaque", Some(now() + 30));
        assert!(is_stale(soon, Duration::from_secs(60)));
        assert!(!is_stale(soon, Duration::from_secs(5)));
    }

    #[test]
    fn the_tokens_own_expiry_wins_over_the_recorded_one() {
        let token = crate::jwt::encode_unsigned(r#"{"exp":4102444800}"#);
        assert_eq!(expires_at(&token, Some(0)), Some(4_102_444_800));
        assert!(!is_stale(
            expires_at(&token, Some(0)),
            Duration::from_secs(60)
        ));
    }
}
