//! Reading the parts of a stored token set the flows need.
//!
//! The document itself is [`keke_credentials::AuthFile`] and the arithmetic is
//! `keke_oauth::token`'s; what lives here joins the two, since `keke-oauth`
//! sits below the credential store and cannot name its types.

use std::time::Duration;

use keke_credentials::AuthTokens;
use keke_oauth::token::TokenResponse;

/// Supplied as a long-lived API key through the credential store rather than
/// minted by a login, so no auth file describes it.
pub(crate) const SOURCE_ENV: &str = "env";

pub(crate) fn expires_at(tokens: &AuthTokens) -> Option<i64> {
    keke_oauth::token::expires_at(&tokens.access_token, tokens.expires_at)
}

pub(crate) fn is_stale(tokens: &AuthTokens, leeway: Duration) -> bool {
    keke_oauth::token::is_stale(expires_at(tokens), leeway)
}

#[cfg(test)]
pub(crate) use keke_oauth::token::now;

/// Fold a token response into a storable credential.
///
/// `previous_refresh` is retained when the issuer omits a refresh token on
/// a refresh — a non-rotating issuer answers with the access token alone,
/// and dropping the refresh token there would turn every refresh into the
/// last one.
/// `issuer` is recorded so the next refresh goes back to whoever minted
/// this, rather than to whatever this build's constant happens to say.
pub(crate) fn into_tokens(
    response: TokenResponse,
    previous_refresh: Option<String>,
    account_id: Option<String>,
    issuer: Option<String>,
) -> AuthTokens {
    AuthTokens {
        expires_at: response.expires_at(),
        access_token: response.access_token,
        refresh_token: response.refresh_token.or(previous_refresh),
        account_id,
        issuer,
    }
}
