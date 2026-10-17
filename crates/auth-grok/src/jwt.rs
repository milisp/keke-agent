//! The claims xAI's tokens carry that this crate reads. Decoding them is
//! `keke_oauth::jwt`'s.

use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
pub(crate) struct Claims {
    pub sub: Option<String>,
    /// What a person recognizes themselves by, and therefore what an account
    /// in a multi-account credential file is named. The login asks for the
    /// `email` scope precisely so this is here to read.
    pub email: Option<String>,
    #[serde(alias = "organization_id")]
    pub org_id: Option<String>,
    /// xAI names the principal a token was minted for, and its token endpoint
    /// expects a refresh to say which principal it is renewing.
    pub principal_type: Option<String>,
    pub principal_id: Option<String>,
}

/// Read the claim set of `token`, or `None` when it is not a JWT at all — an
/// API key, for instance, which has no claims.
pub(crate) fn claims(token: &str) -> Option<Claims> {
    keke_oauth::jwt::claims(token)
}

#[cfg(test)]
pub(crate) use keke_oauth::jwt::encode_unsigned;
