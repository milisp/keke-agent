//! The xAI Grok model provider.
//!
//! xAI serves the OpenAI `/chat/completions` schema, so everything about the
//! translation and the stream is shared with every other vendor that does —
//! [`keke_wire`] owns it. What is left here, and all that should be here, is
//! the vendor's identity: where it lives, what credential it uses, and which
//! wire format it speaks.

use std::sync::Arc;

use keke_auth_api::AuthProvider;
use keke_provider_api::ModelInfo;
use keke_provider_api::ModelProvider;
use keke_provider_api::ModelRequest;
use keke_provider_api::ProviderError;
use keke_provider_api::ProviderFuture;
use keke_provider_api::ProviderInfo;
use keke_provider_api::StreamEvent;
use keke_provider_api::WireApi;
use keke_wire::WireClient;

/// The public xAI endpoint. Overridable per deployment through
/// [`GrokProvider::new`], which is how a proxy or a test server is pointed at.
const DEFAULT_BASE_URL: &str = "https://api.x.ai/v1";

/// xAI's Grok models over the chat-completions wire.
pub struct GrokProvider {
    info: ProviderInfo,
    wire: WireClient,
}

impl GrokProvider {
    #[must_use]
    pub fn new(auth: Arc<dyn AuthProvider>, base_url: Option<String>) -> Self {
        let base_url = base_url
            .filter(|url| !url.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        let wire = WireClient::new(base_url, auth);
        Self {
            info: ProviderInfo {
                route: "grok".to_string(),
                display_name: "xAI Grok".to_string(),
                base_url: wire.base_url().to_string(),
                wire_api: WireApi::ChatCompletions,
                auth_id: Some("grok".to_string()),
                env_key: Some("XAI_API_KEY".to_string()),
            },
            wire,
        }
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
        Box::pin(self.wire.stream(self.info.wire_api, request))
    }

    /// xAI can enumerate its models, so a failure here is reported rather than
    /// flattened to an empty list: the empty list means "this provider cannot
    /// enumerate", and returning it for a rejected key would present an
    /// authentication problem as an account with no models.
    fn list_models(&self) -> ProviderFuture<'_, Result<Vec<ModelInfo>, ProviderError>> {
        Box::pin(self.wire.list_models())
    }
}

#[cfg(test)]
mod tests;
