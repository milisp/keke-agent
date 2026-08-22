//! Reading the parts of a stored token set the flows need.
//!
//! The document itself is [`keke_credentials::AuthFile`]; what lives here is
//! the arithmetic around it — when a token stops working, and how a token
//! endpoint response folds into one.

use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use keke_credentials::AuthTokens;
use serde::Deserialize;

/// Supplied as a long-lived API key through the credential store rather than
/// minted by a login, so no auth file describes it.
pub(crate) const SOURCE_ENV: &str = "env";

/// When this credential stops working, preferring the token's own `exp` over
/// the recorded one: a refresh that reused the stored `expires_at` would keep
/// reporting the original login's deadline forever.
pub(crate) fn expires_at(tokens: &AuthTokens) -> Option<i64> {
    crate::jwt::claims(&tokens.access_token)
        .and_then(|claims| claims.exp)
        .or(tokens.expires_at)
}

/// Whether the token is expired or close enough that a request made now might
/// arrive after it is not.
///
/// A credential with no stated expiry is never stale: an API key is not
/// refreshable and treating it as expired would loop.
pub(crate) fn is_stale(tokens: &AuthTokens, leeway: Duration) -> bool {
    match expires_at(tokens) {
        Some(exp) => exp <= now() + leeway.as_secs() as i64,
        None => false,
    }
}

pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

/// A successful response from the token endpoint, in either flow.
#[derive(Debug, Deserialize)]
pub(crate) struct TokenResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: Option<i64>,
}

impl TokenResponse {
    /// Fold a token response into a storable credential.
    ///
    /// `previous_refresh` is retained when the issuer omits a refresh token on
    /// a refresh — a non-rotating issuer answers with the access token alone,
    /// and dropping the refresh token there would turn every refresh into the
    /// last one.
    pub(crate) fn into_tokens(
        self,
        previous_refresh: Option<String>,
        account_id: Option<String>,
    ) -> AuthTokens {
        AuthTokens {
            access_token: self.access_token,
            refresh_token: self.refresh_token.or(previous_refresh),
            account_id,
            expires_at: self.expires_in.map(|seconds| now() + seconds),
        }
    }
}

/// An OAuth2 error response, per RFC 6749 §5.2.
#[derive(Debug, Deserialize)]
pub(crate) struct TokenError {
    pub error: String,
    pub error_description: Option<String>,
}

impl TokenError {
    pub(crate) fn detail(&self) -> &str {
        self.error_description.as_deref().unwrap_or(&self.error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_credential_without_an_expiry_is_never_stale() {
        let tokens = AuthTokens::bearer("openai-key");
        assert!(!is_stale(&tokens, Duration::from_secs(60)));
    }

    #[test]
    fn the_leeway_makes_a_soon_to_expire_token_stale() {
        let tokens = AuthTokens {
            expires_at: Some(now() + 30),
            ..AuthTokens::bearer("opaque")
        };
        assert!(is_stale(&tokens, Duration::from_secs(60)));
        assert!(!is_stale(&tokens, Duration::from_secs(5)));
    }

    #[test]
    fn the_tokens_own_expiry_wins_over_the_recorded_one() {
        let tokens = AuthTokens {
            expires_at: Some(0),
            ..AuthTokens::bearer(crate::jwt::encode_unsigned(r#"{"exp":4102444800}"#))
        };
        assert_eq!(expires_at(&tokens), Some(4_102_444_800));
        assert!(!is_stale(&tokens, Duration::from_secs(60)));
    }
}
