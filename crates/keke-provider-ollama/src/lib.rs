//! The Ollama model provider.
//!
//! Ollama serves the OpenAI-compatible `/chat/completions` wire format and
//! `/models` listing. No authentication is required by default for a local
//! instance. The default base URL is `http://localhost:11434/v1`.

mod catalog;

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

/// The route key this provider registers under.
pub const ROUTE: &str = "ollama";

/// The default local Ollama endpoint.
pub const DEFAULT_BASE_URL: &str = "http://localhost:11434/v1";

/// How to reach Ollama for one session.
pub struct Endpoint {
    pub base_url: String,
    pub wire_api: WireApi,
}

impl Default for Endpoint {
    /// The local Ollama server, which is what a session without a configured
    /// credential reaches.
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            wire_api: WireApi::ChatCompletions,
        }
    }
}

/// Ollama models.
pub struct OllamaProvider {
    info: ProviderInfo,
    wire: WireClient,
}

impl OllamaProvider {
    #[must_use]
    pub fn new(auth: Arc<dyn AuthProvider>, endpoint: Endpoint) -> Self {
        let base_url = if endpoint.base_url.trim().is_empty() {
            DEFAULT_BASE_URL.to_string()
        } else {
            endpoint.base_url
        };
        let wire = WireClient::new(base_url, auth);
        Self {
            info: ProviderInfo {
                route: ROUTE.to_string(),
                display_name: "Ollama".to_string(),
                base_url: wire.base_url().to_string(),
                wire_api: endpoint.wire_api,
                auth_id: None,
                env_key: None,
            },
            wire,
        }
    }
}

impl ModelProvider for OllamaProvider {
    fn info(&self) -> &ProviderInfo {
        &self.info
    }

    fn stream<'a>(
        &'a self,
        request: ModelRequest,
    ) -> ProviderFuture<'a, Result<StreamEvent, ProviderError>> {
        Box::pin(self.wire.stream(self.info.wire_api, request))
    }

    /// What Ollama serves. No caching — the local endpoint is fast and always
    /// available when running.
    fn list_models(&self) -> ProviderFuture<'_, Result<Vec<ModelInfo>, ProviderError>> {
        Box::pin(self.wire.list_models(self.info.wire_api))
    }
}
