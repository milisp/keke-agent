//! Just enough JWT to know when an access token is about to stop working.
//!
//! Deliberately not a JWT library: nothing here verifies a signature, because
//! the client is not the audience that would check one. It reads the claims the
//! issuer already told us are true so a refresh can happen before a request
//! fails rather than after.
//!
//! Which claims matter beyond `exp` is the issuer's business, so the claim set
//! is the caller's type.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use serde::de::DeserializeOwned;

/// Read the claim set of `token`, or `None` when it is not a JWT at all — an
/// API key, for instance, which never expires and has no claims.
pub fn claims<T: DeserializeOwned>(token: &str) -> Option<T> {
    let payload = token
        .split('.')
        .nth(1)
        .filter(|_| token.matches('.').count() == 2)?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| URL_SAFE.decode(payload))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The token's own `exp`, when it is a JWT that states one.
pub fn expiry(token: &str) -> Option<i64> {
    #[derive(Deserialize)]
    struct Exp {
        exp: Option<i64>,
    }
    claims::<Exp>(token).and_then(|claims| claims.exp)
}

/// An unsigned token carrying `payload`, for tests that need a JWT-shaped
/// credential without an issuer to mint one.
#[cfg(any(test, feature = "test-support"))]
pub fn encode_unsigned(payload: &str) -> String {
    let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"RS256","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(payload);
    format!("{header}.{payload}.not-a-signature")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct Claims {
        sub: Option<String>,
    }

    #[test]
    fn reads_expiry_and_the_callers_claims() {
        let token = encode_unsigned(r#"{"exp":1772575524,"sub":"user-1","aud":["x"]}"#);
        assert_eq!(expiry(&token), Some(1772575524));
        assert_eq!(
            claims::<Claims>(&token).unwrap().sub.as_deref(),
            Some("user-1")
        );
    }

    #[test]
    fn a_non_jwt_credential_has_no_claims() {
        assert!(expiry("sk-abcdef").is_none());
        assert!(expiry("a.b").is_none());
    }
}
