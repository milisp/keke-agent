//! The Ollama model provider.
//!
//! Ollama serves the OpenAI-compatible `/chat/completions` wire format and
//! `/models` listing. No authentication is required by default for a local
//! instance. The default base URL is `http://localhost:11434/v1`.

mod catalog;

use std::sync::Arc;

use keke_auth_api::AuthProvider;
use keke_catalog::CatalogCache;
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

/// The answer `POST /api/show` gives, narrowed to what we read.
#[derive(Debug, serde::Deserialize)]
struct ShowResponse {
    #[serde(default)]
    context_length: Option<u64>,
}

/// Ollama models.
pub struct OllamaProvider {
    info: ProviderInfo,
    wire: WireClient,
    /// `None` disables caching entirely, which is what a surface with no home
    /// directory — a test — gets.
    cache: Option<CatalogCache>,
}

impl OllamaProvider {
    #[must_use]
    pub fn new(
        auth: Arc<dyn AuthProvider>,
        endpoint: Endpoint,
        cache: Option<CatalogCache>,
    ) -> Self {
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
            cache,
        }
    }

    /// Ask the endpoint what it serves.
    async fn fetch(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let models = self.wire.list_models(self.info.wire_api).await?;
        Ok(self.with_context_windows(models).await)
    }

    /// Fill in each model's context window from the native API.
    ///
    /// The OpenAI-shaped `/v1/models` listing carries no context size, but
    /// `POST /api/show` answers `context_length` for one model. Best-effort per
    /// model: a server that declines one model still leaves the others sized,
    /// and a failure anywhere leaves `None` — which the interface already
    /// renders as "unknown" rather than as zero.
    async fn with_context_windows(&self, mut models: Vec<ModelInfo>) -> Vec<ModelInfo> {
        // The native API lives at the server root; the configured endpoint is
        // the OpenAI-compatible one beneath `/v1`.
        let root = self
            .wire
            .base_url()
            .trim_end_matches('/')
            .trim_end_matches("/v1");
        for model in &mut models {
            let body = serde_json::json!({ "model": model.id });
            if let Ok(text) = self.wire.post(&format!("{root}/api/show"), &body).await
                && let Ok(shown) = serde_json::from_str::<ShowResponse>(&text)
            {
                model.context_window = shown.context_length;
            }
        }
        models
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

    /// What Ollama serves, from the cache when it is current and from the
    /// local server otherwise. A local endpoint is usually fast, but it is
    /// only fast when it is running — and "not running" arrives as a connect
    /// timeout on the path of every session start, which a cache turns into
    /// the last list the server gave rather than a stalled picker.
    ///
    /// Unlike the hosted vendors there is no compiled-in catalog to fall back
    /// to — what Ollama serves is whatever that machine has pulled — so an
    /// unreachable server with no stored list yields an empty picker, and the
    /// interface tells the person to type a model name instead.
    fn list_models(&self) -> ProviderFuture<'_, Result<Vec<ModelInfo>, ProviderError>> {
        Box::pin(async move {
            let cached = self.cache.as_ref().and_then(|cache| cache.load(ROUTE));
            if let Some(cached) = &cached
                && cached.fresh
            {
                return Ok(cached.models.clone());
            }
            match self.fetch().await {
                Ok(models) if !models.is_empty() => {
                    if let Some(cache) = &self.cache {
                        cache.store(ROUTE, &models);
                    }
                    Ok(models)
                }
                // An empty or failed listing is not cached: one request while
                // the server was down must not hold the picker empty for a
                // whole TTL once it is back up.
                Ok(_) | Err(_) => Ok(cached.map(|cached| cached.models).unwrap_or_default()),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keke_auth_api::AuthError;
    use keke_auth_api::AuthFuture;
    use keke_auth_api::CredentialSnapshot;

    #[derive(Default)]
    struct StubAuth;

    impl AuthProvider for StubAuth {
        fn id(&self) -> &'static str {
            "ollama"
        }

        fn snapshot(&self) -> CredentialSnapshot {
            CredentialSnapshot {
                auth_id: "ollama".to_string(),
                source: "test".to_string(),
                ..CredentialSnapshot::default()
            }
        }

        fn headers(&self) -> AuthFuture<'_, Result<keke_auth_api::AuthHeaders, AuthError>> {
            Box::pin(async { Ok(keke_auth_api::AuthHeaders::bearer("token")) })
        }

        fn login<'a>(
            &'a self,
            _ui: Arc<dyn keke_auth_api::LoginUi>,
        ) -> AuthFuture<'a, Result<(), AuthError>> {
            Box::pin(async { Ok(()) })
        }

        fn refresh_after_unauthorized(&self) -> AuthFuture<'_, bool> {
            Box::pin(async { false })
        }

        fn logout(&self) -> AuthFuture<'_, Result<(), AuthError>> {
            Box::pin(async { Ok(()) })
        }
    }

    fn cache() -> (tempfile::TempDir, CatalogCache) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = keke_paths::AbsPath::new(dir.path()).expect("absolute");
        let cache = CatalogCache::new(&path, std::time::Duration::from_secs(3600));
        (dir, cache)
    }

    #[tokio::test]
    async fn lists_from_the_cache_without_touching_the_server() {
        let (_dir, cache) = cache();
        cache.store(
            ROUTE,
            &[
                ModelInfo::new("llama3.1:8b"),
                ModelInfo::new("qwen2.5-coder:7b"),
            ],
        );
        // No reachable server behind this address; only the cache answers.
        let provider = OllamaProvider::new(Arc::new(StubAuth), Endpoint::default(), Some(cache));
        let models = provider.list_models().await.expect("listed");
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "llama3.1:8b");
    }

    #[tokio::test]
    async fn an_unreachable_server_with_no_cache_yields_an_empty_list() {
        // Port 9 (discard) so the test does not depend on whether a real
        // Ollama happens to be running at the default address.
        let provider = OllamaProvider::new(
            Arc::new(StubAuth),
            Endpoint {
                base_url: "http://127.0.0.1:9/v1".to_string(),
                wire_api: WireApi::ChatCompletions,
            },
            None,
        );
        assert!(provider.list_models().await.expect("no error").is_empty());
    }
}
