//! Turning HTTP outcomes into the [`ProviderError`] variants the engine's retry
//! policy is written against.
//!
//! The classification is shared across all three wire formats on purpose: an
//! engine deciding whether to back off, refresh credentials, or surface an error
//! must not get a different answer depending on which schema the vendor happens
//! to speak.

use std::time::Duration;

use keke_provider_api::ProviderError;

/// A failure that happened before any response arrived.
pub(crate) fn transport_error(error: reqwest::Error) -> ProviderError {
    if error.is_timeout() || error.is_connect() || error.is_request() {
        ProviderError::Transient(error.to_string())
    } else {
        ProviderError::Protocol(error.to_string())
    }
}

/// Turn a non-2xx response into the variant the engine's retry policy expects.
///
/// The distinctions matter: the engine retries `RateLimited` and `Transient`
/// with backoff, refreshes credentials once for `Unauthorized`, and surfaces
/// `InvalidRequest` immediately because retrying it unchanged cannot help.
pub(crate) async fn check_status(
    response: reqwest::Response,
) -> Result<reqwest::Response, ProviderError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let retry_after = retry_after_millis(&response);
    let body = response.text().await.unwrap_or_default();
    let detail = error_detail(&body);
    Err(match status.as_u16() {
        401 => ProviderError::Unauthorized(detail),
        // A 403 is the ambiguous one: vendors use it both for a rejected
        // credential and for an account that has run out of credits. Only the
        // former is worth refreshing a token over.
        403 if names_an_auth_failure(&body) => ProviderError::Unauthorized(detail),
        402 | 403 => ProviderError::NotEntitled(detail),
        404 => ProviderError::UnknownModel(detail),
        429 => ProviderError::RateLimited {
            retry_after_millis: retry_after,
        },
        400 | 422 => ProviderError::InvalidRequest(detail),
        code if (500..600).contains(&code) => ProviderError::Transient(detail),
        code => ProviderError::Protocol(format!("provider returned HTTP {code}: {detail}")),
    })
}

/// Whether a 403's body blames the credential rather than the account.
///
/// xAI answers an out-of-credits account with
/// `personal-team-blocked:spending-limit`, which is not something a token
/// refresh can fix; it answers a bad credential with `unauthenticated`.
fn names_an_auth_failure(body: &str) -> bool {
    const AUTH_MARKERS: &[&str] = &[
        "unauthenticated",
        "unauthorized",
        "invalid_api_key",
        "invalid api key",
        "invalid_token",
        "invalid token",
        "expired",
        "missing",
        "revoked",
    ];
    let body = body.to_ascii_lowercase();
    AUTH_MARKERS.iter().any(|marker| body.contains(marker))
}

/// `retry-after` is seconds or an HTTP date; only the seconds form carries a
/// delay we can honor without a clock dependency.
fn retry_after_millis(response: &reqwest::Response) -> Option<u64> {
    let value = response.headers().get("retry-after")?.to_str().ok()?;
    let seconds: f64 = value.trim().parse().ok()?;
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    Some(
        Duration::from_secs_f64(seconds)
            .as_millis()
            .min(u128::from(u64::MAX)) as u64,
    )
}

/// Vendors report failures as `{"error": {"message": ..}}`, as
/// `{"error": ".."}`, and as plain text from an edge that never reached them.
/// Keep whatever is there rather than inventing a generic sentence.
pub(crate) fn error_detail(body: &str) -> String {
    let parsed: Option<serde_json::Value> = serde_json::from_str(body).ok();
    let message = parsed.as_ref().and_then(|value| match value.get("error") {
        Some(serde_json::Value::String(text)) => Some(text.clone()),
        Some(object) => object
            .get("message")
            .and_then(|m| m.as_str())
            .map(str::to_string),
        None => value
            .get("message")
            .and_then(|m| m.as_str())
            .map(str::to_string),
    });
    message.unwrap_or_else(|| body.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verbatim from `GET https://api.x.ai/v1/models` with a valid xAI
    /// subscription token on an account with no credits.
    const XAI_OUT_OF_CREDITS: &str = r#"{"code":"personal-team-blocked:spending-limit","error":"You have run out of credits or need a Grok subscription. Add credits at https://grok.com/?_s=usage or upgrade at https://grok.com/supergrok."}"#;

    #[test]
    fn an_out_of_credits_403_is_not_an_authentication_failure() {
        assert!(!names_an_auth_failure(XAI_OUT_OF_CREDITS));

        let error = ProviderError::NotEntitled(error_detail(XAI_OUT_OF_CREDITS).to_string());
        assert!(
            !error.needs_reauth(),
            "refreshing a token buys no credits, and the retry hides the account message"
        );
        assert!(!error.is_retryable());
        assert!(
            error.to_string().contains("run out of credits"),
            "the vendor's own words are what tell a person what to do: {error}"
        );
    }

    #[test]
    fn a_403_that_blames_the_credential_still_refreshes() {
        let body = r#"{"code":"unauthenticated","error":"API key is missing."}"#;
        assert!(names_an_auth_failure(body));
    }
}
