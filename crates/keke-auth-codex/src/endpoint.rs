use std::time::Duration;

use keke_auth_api::AuthError;
use reqwest::Client;

/// A token request runs while the credential's mutation lock is held, and that
/// lock is broken as abandoned after a minute. A request that outlived it would
/// be writing its answer into a file another process had taken over.
const TOKEN_TIMEOUT: Duration = Duration::from_secs(15);

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
    read_token_response(http.post(url).form(form)).await
}

/// The same exchange with a JSON body.
///
/// The refusal is not symmetric with the form encoding: OpenAI's token endpoint
/// answers a form-encoded refresh with an error, so a refresh that renews
/// nothing looks exactly like a revoked credential. The authorization-code
/// exchange stays form-encoded because that is what the same endpoint wants
/// there.
pub(crate) async fn post_token_json(
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
pub(crate) async fn exchange(
    http: &Client,
    url: &str,
    form: &[(&str, &str)],
) -> Result<TokenResponse, AuthError> {
    terminal(post_token(http, url, form).await?)
}

/// [`exchange`] with a JSON body — see [`post_token_json`].
pub(crate) async fn exchange_json(
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
