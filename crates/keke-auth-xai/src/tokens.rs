use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use serde::Deserialize;
use serde::Serialize;

/// Obtained through the loopback authorization-code flow.
pub(crate) const SOURCE_OAUTH: &str = "oauth";
/// Obtained through the RFC 8628 device authorization grant.
pub(crate) const SOURCE_DEVICE_CODE: &str = "device-code";
/// Supplied as a long-lived API key rather than minted by a login.
pub(crate) const SOURCE_ENV: &str = "env";

/// The credential as it is written to the store.
///
/// One JSON document under one reference rather than three references, so a
/// half-written login cannot leave an access token paired with the previous
/// refresh token.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct StoredTokens {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Unix seconds, recorded from `expires_in` for issuers whose access token
    /// is opaque and therefore carries no readable `exp`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    pub source: String,
}

impl StoredTokens {
    /// When this credential stops working, preferring the token's own `exp`
    /// over the recorded one: a refresh that reused the stored `expires_at`
    /// would keep reporting the original login's deadline forever.
    pub(crate) fn expires_at(&self) -> Option<i64> {
        crate::jwt::claims(&self.access_token)
            .and_then(|claims| claims.exp)
            .or(self.expires_at)
    }

    /// Whether the token is expired or close enough that a request made now
    /// might arrive after it is not.
    ///
    /// A credential with no stated expiry is never stale: an API key is not
    /// refreshable and treating it as expired would loop.
    pub(crate) fn is_stale(&self, leeway: Duration) -> bool {
        match self.expires_at() {
            Some(exp) => exp <= now() + leeway.as_secs() as i64,
            None => false,
        }
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
    pub(crate) fn into_stored(
        self,
        source: &str,
        previous_refresh: Option<String>,
    ) -> StoredTokens {
        StoredTokens {
            access_token: self.access_token,
            refresh_token: self.refresh_token.or(previous_refresh),
            expires_at: self.expires_in.map(|seconds| now() + seconds),
            source: source.to_string(),
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
        let tokens = StoredTokens {
            access_token: "xai-key".into(),
            refresh_token: None,
            expires_at: None,
            source: SOURCE_ENV.into(),
        };
        assert!(!tokens.is_stale(Duration::from_secs(60)));
    }

    #[test]
    fn the_leeway_makes_a_soon_to_expire_token_stale() {
        let tokens = StoredTokens {
            access_token: "opaque".into(),
            refresh_token: None,
            expires_at: Some(now() + 30),
            source: SOURCE_OAUTH.into(),
        };
        assert!(tokens.is_stale(Duration::from_secs(60)));
        assert!(!tokens.is_stale(Duration::from_secs(5)));
    }

    #[test]
    fn the_tokens_own_expiry_wins_over_the_recorded_one() {
        let tokens = StoredTokens {
            access_token: crate::jwt::encode_unsigned(r#"{"exp":4102444800}"#),
            refresh_token: None,
            expires_at: Some(0),
            source: SOURCE_OAUTH.into(),
        };
        assert_eq!(tokens.expires_at(), Some(4102444800));
        assert!(!tokens.is_stale(Duration::from_secs(60)));
    }
}
