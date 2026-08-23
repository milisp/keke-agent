//! Ported from codex: `codex-rs/login/src/server.rs`.
//!
//! Upstream owns this flow's shape because upstream owns the client
//! registration it is validated against. The authorize endpoint accepts one
//! redirect URI, and refuses a request missing either of the two flags below
//! with `invalid_authorize_request` — facts about OpenAI's registration, not
//! about OAuth2, so re-deriving them from the spec produces a login that
//! cannot work. They are copied rather than reasoned out, and they change when
//! upstream changes.
//!
//! Adapted: `scope` and `originator` are parameters here, because
//! `keke-config-types` is where a deployment states a value it might need to
//! change (`AGENTS.md` invariant 9); upstream inlines both. The query order and
//! encoding are upstream's.

use std::fmt::Write as _;

/// The loopback port this client's redirect URI is registered at.
///
/// Upstream: `DEFAULT_PORT`.
pub(crate) const DEFAULT_PORT: u16 = 1455;

/// The path half of that same registration. Upstream builds it inline.
pub(crate) const CALLBACK_PATH: &str = "/auth/callback";

/// `localhost`, not `127.0.0.1`: the registration names the host, and the two
/// are not interchangeable to the issuer even though they resolve to the same
/// socket.
pub(crate) fn redirect_uri(port: u16) -> String {
    format!("http://localhost:{port}{CALLBACK_PATH}")
}

/// Upstream: `build_authorize_url`.
pub(crate) fn build_authorize_url(
    authorize_endpoint: &str,
    client_id: &str,
    redirect_uri: &str,
    scope: &str,
    code_challenge: &str,
    state: &str,
    originator: &str,
) -> String {
    let query = [
        ("response_type", "code"),
        ("client_id", client_id),
        ("redirect_uri", redirect_uri),
        ("scope", scope),
        ("code_challenge", code_challenge),
        ("code_challenge_method", "S256"),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("state", state),
        ("originator", originator),
    ];

    let mut qs = String::new();
    for (key, value) in query {
        if !qs.is_empty() {
            qs.push('&');
        }
        let encoded: String = url::form_urlencoded::byte_serialize(value.as_bytes()).collect();
        let _ = write!(qs, "{key}={encoded}");
    }
    format!("{authorize_endpoint}?{qs}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two flags and the registered redirect are the whole reason this file
    /// exists: a request without them is refused as `invalid_authorize_request`.
    #[test]
    fn the_url_carries_what_the_client_registration_expects() {
        let url = build_authorize_url(
            "https://auth.openai.com/oauth/authorize",
            "app_1",
            &redirect_uri(DEFAULT_PORT),
            "openid profile",
            "challenge",
            "state-1",
            "codex_cli_rs",
        );

        assert!(url.contains("id_token_add_organizations=true"), "{url}");
        assert!(url.contains("codex_cli_simplified_flow=true"), "{url}");
        assert!(url.contains("originator=codex_cli_rs"), "{url}");
        assert!(
            url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback"),
            "{url}"
        );
    }
}
