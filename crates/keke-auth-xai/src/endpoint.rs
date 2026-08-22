use keke_auth_api::AuthError;
use reqwest::Client;

use crate::tokens::TokenError;
use crate::tokens::TokenResponse;

/// What the token endpoint said.
///
/// `Refused` is a value rather than an error because the device-code flow reads
/// `authorization_pending` and `slow_down` as *progress*; collapsing them into
/// an error type would mean parsing an error message back into control flow.
pub(crate) enum TokenOutcome {
    Granted(TokenResponse),
    Refused(TokenError),
}

pub(crate) async fn post_token(
    http: &Client,
    url: &str,
    form: &[(&str, &str)],
) -> Result<TokenOutcome, AuthError> {
    let response = http
        .post(url)
        .form(form)
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
pub(crate) async fn exchange(
    http: &Client,
    url: &str,
    form: &[(&str, &str)],
) -> Result<TokenResponse, AuthError> {
    match post_token(http, url, form).await? {
        TokenOutcome::Granted(tokens) => Ok(tokens),
        TokenOutcome::Refused(err) => Err(AuthError::Rejected(err.detail().to_string())),
    }
}
