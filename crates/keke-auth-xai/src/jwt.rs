//! Just enough JWT to know when an access token is about to stop working.
//!
//! Deliberately not a JWT library: nothing here verifies a signature, because
//! the client is not the audience that would check one. It reads the claims the
//! issuer already told us are true so a refresh can happen before a request
//! fails rather than after.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
pub(crate) struct Claims {
    pub exp: Option<i64>,
    pub sub: Option<String>,
    #[serde(alias = "organization_id")]
    pub org_id: Option<String>,
}

/// Read the claim set of `token`, or `None` when it is not a JWT at all — an
/// API key, for instance, which never expires and has no claims.
pub(crate) fn claims(token: &str) -> Option<Claims> {
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

#[cfg(test)]
pub(crate) fn encode_unsigned(payload: &str) -> String {
    let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"RS256","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(payload);
    format!("{header}.{payload}.not-a-signature")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_expiry_and_subject() {
        let token = encode_unsigned(r#"{"exp":1772575524,"sub":"user-1","aud":["x"]}"#);
        let claims = claims(&token).unwrap();
        assert_eq!(claims.exp, Some(1772575524));
        assert_eq!(claims.sub.as_deref(), Some("user-1"));
    }

    #[test]
    fn a_non_jwt_credential_has_no_claims() {
        assert!(claims("xai-abcdef").is_none());
        assert!(claims("a.b").is_none());
    }
}
