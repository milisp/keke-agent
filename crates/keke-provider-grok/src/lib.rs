//! The xAI Grok model provider.
//!
//! xAI serves the OpenAI `/chat/completions` schema, so the wire translation
//! here is deliberately plain; what is specific to this crate is the mapping
//! back into the neutral vocabulary and the error classification, which the
//! engine's retry policy depends on being precise.

mod request;
mod sse;

use std::sync::Arc;
use std::time::Duration;

use eventsource_stream::Eventsource;
use futures::StreamExt;
use futures::TryStreamExt;
use keke_auth_api::AuthProvider;
use keke_provider_api::ModelInfo;
use keke_provider_api::ModelProvider;
use keke_provider_api::ModelRequest;
use keke_provider_api::ProviderError;
use keke_provider_api::ProviderFuture;
use keke_provider_api::ProviderInfo;
use keke_provider_api::StreamEvent;
use keke_provider_api::WireApi;
use serde::Deserialize;

/// The public xAI endpoint. Overridable per deployment through
/// [`GrokProvider::new`], which is how a proxy or a test server is pointed at.
const DEFAULT_BASE_URL: &str = "https://api.x.ai/v1";

/// xAI's Grok models over the chat-completions wire.
pub struct GrokProvider {
    info: ProviderInfo,
    auth: Arc<dyn AuthProvider>,
    http: reqwest::Client,
}

impl GrokProvider {
    #[must_use]
    pub fn new(auth: Arc<dyn AuthProvider>, base_url: Option<String>) -> Self {
        let base_url = base_url
            .filter(|url| !url.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        Self {
            info: ProviderInfo {
                route: "grok".to_string(),
                display_name: "xAI Grok".to_string(),
                base_url: base_url.trim_end_matches('/').to_string(),
                wire_api: WireApi::ChatCompletions,
                auth_id: Some("grok".to_string()),
                env_key: Some("XAI_API_KEY".to_string()),
            },
            auth,
            http: reqwest::Client::new(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.info.base_url)
    }

    /// Attach credentials to `builder`.
    ///
    /// Headers are fetched per request and never held between them, so a token
    /// refreshed after a 401 reaches the very next call.
    async fn authorize(
        &self,
        builder: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder, ProviderError> {
        let headers = self
            .auth
            .headers()
            .await
            .map_err(|error| ProviderError::Unauthorized(error.to_string()))?;
        Ok(headers.iter().fold(builder, |builder, (name, value)| {
            builder.header(name, value)
        }))
    }
}

impl ModelProvider for GrokProvider {
    fn info(&self) -> &ProviderInfo {
        &self.info
    }

    fn stream<'a>(
        &'a self,
        request: ModelRequest,
    ) -> ProviderFuture<'a, Result<StreamEvent, ProviderError>> {
        Box::pin(async move {
            let body = request::chat_completions_body(&request, true);
            let builder = self
                .http
                .post(self.url("/chat/completions"))
                .header("accept", "text/event-stream")
                .json(&body);
            let response = self
                .authorize(builder)
                .await?
                .send()
                .await
                .map_err(transport_error)?;
            let response = check_status(response).await?;

            let frames = response
                .bytes_stream()
                .eventsource()
                // A break mid-stream is a transport failure, not a malformed
                // reply: the engine may retry it.
                .map_err(|error| ProviderError::Transient(error.to_string()))
                .map_ok(|event| event.data)
                .boxed();
            Ok(sse::decode(frames))
        })
    }

    /// xAI can enumerate its models, so a failure here is reported rather than
    /// flattened to an empty list: the empty list means "this provider cannot
    /// enumerate", and returning it for a rejected key would present an
    /// authentication problem as an account with no models.
    fn list_models(&self) -> ProviderFuture<'_, Result<Vec<ModelInfo>, ProviderError>> {
        Box::pin(async move {
            let builder = self.http.get(self.url("/models"));
            let response = self
                .authorize(builder)
                .await?
                .send()
                .await
                .map_err(transport_error)?;
            let body = check_status(response)
                .await?
                .text()
                .await
                .map_err(|error| ProviderError::Protocol(error.to_string()))?;
            let listing: ModelListing = serde_json::from_str(&body).map_err(|error| {
                ProviderError::Protocol(format!("xAI sent an undecodable model list: {error}"))
            })?;
            Ok(listing.data.into_iter().map(ModelInfo::from).collect())
        })
    }
}

#[derive(Debug, Deserialize)]
struct ModelListing {
    #[serde(default)]
    data: Vec<WireModel>,
}

#[derive(Debug, Deserialize)]
struct WireModel {
    id: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    context_window: Option<u64>,
    #[serde(default)]
    max_output_tokens: Option<u64>,
    /// Present on xAI's richer model listings; absent on the plain one.
    #[serde(default)]
    input_modalities: Option<Vec<String>>,
}

impl From<WireModel> for ModelInfo {
    fn from(wire: WireModel) -> Self {
        let supports_vision = wire
            .input_modalities
            .as_ref()
            .is_some_and(|modalities| modalities.iter().any(|kind| kind == "image"));
        Self {
            display_name: wire.display_name.unwrap_or_else(|| wire.id.clone()),
            id: wire.id,
            context_window: wire.context_window,
            max_output_tokens: wire.max_output_tokens,
            // Every Grok model on this endpoint accepts tools and exposes
            // reasoning; the listing does not say so, and claiming otherwise
            // would make the engine withhold tools it could have sent.
            supports_tools: true,
            supports_vision,
            supports_reasoning: true,
        }
    }
}

/// A failure before any response arrived.
fn transport_error(error: reqwest::Error) -> ProviderError {
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
async fn check_status(response: reqwest::Response) -> Result<reqwest::Response, ProviderError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let retry_after = retry_after_millis(&response);
    let body = response.text().await.unwrap_or_default();
    let detail = error_detail(&body);
    Err(match status.as_u16() {
        401 | 403 => ProviderError::Unauthorized(detail),
        404 => ProviderError::UnknownModel(detail),
        429 => ProviderError::RateLimited {
            retry_after_millis: retry_after,
        },
        400 | 422 => ProviderError::InvalidRequest(detail),
        code if (500..600).contains(&code) => ProviderError::Transient(detail),
        code => ProviderError::Protocol(format!("xAI returned HTTP {code}: {detail}")),
    })
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

/// xAI reports failures as `{"error": {"message": ..}}` or `{"error": ".."}`,
/// and as plain text from its edge. Keep whatever is there.
fn error_detail(body: &str) -> String {
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
mod tests;
