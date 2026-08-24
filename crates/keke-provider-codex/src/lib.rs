//! The OpenAI model provider.
//!
//! One vendor with two addresses. A ChatGPT login is spent at the subscription
//! backend, which fixes its own sampling and publishes a catalog rich enough to
//! draw a picker from; an API key is spent at the public API, which takes a
//! reply budget and answers `/models` with a bag of ids. Which one a session
//! talks to follows the credential, and the composition root is the only place
//! that knows which credential is stored — so it passes the address in rather
//! than this crate guessing.
//!
//! What is here and nowhere else is the vendor's identity and the shape of its
//! catalog. Everything about the request and the stream is `keke-wire`'s.

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

/// The route key this provider registers under, and the auth id it draws its
/// credentials from.
pub const ROUTE: &str = "codex";

/// OpenAI over the responses wire.
pub struct CodexProvider {
    info: ProviderInfo,
    wire: WireClient,
    /// `None` disables caching entirely, which is what a surface with no home
    /// directory — a test — gets.
    cache: Option<CatalogCache>,
}

/// How to reach OpenAI for one session.
pub struct Endpoint {
    /// Where to send requests. The composition root picks it from the stored
    /// credential, because a subscription token at the public API and an API
    /// key at the subscription backend both fail as a 401 that names neither
    /// the address nor the account.
    pub base_url: String,
    /// Whether this address refuses a request that names a reply budget or a
    /// temperature, as the subscription backend does.
    pub fixed_sampling: bool,
}

impl CodexProvider {
    #[must_use]
    pub fn new(
        auth: Arc<dyn AuthProvider>,
        endpoint: Endpoint,
        cache: Option<CatalogCache>,
    ) -> Self {
        let mut wire = WireClient::new(endpoint.base_url, auth);
        if endpoint.fixed_sampling {
            wire = wire.with_fixed_sampling();
        }
        Self {
            info: ProviderInfo {
                route: ROUTE.to_string(),
                display_name: "OpenAI Codex".to_string(),
                base_url: wire.base_url().to_string(),
                wire_api: WireApi::Responses,
                auth_id: Some(ROUTE.to_string()),
                env_key: Some("OPENAI_API_KEY".to_string()),
            },
            wire,
            cache,
        }
    }

    /// Ask the endpoint what it serves.
    async fn fetch(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let body = self.wire.fetch("/models").await?;
        catalog::parse(&body)
            .map_err(|error| ProviderError::Protocol(format!("undecodable model list: {error}")))
    }
}

impl ModelProvider for CodexProvider {
    fn info(&self) -> &ProviderInfo {
        &self.info
    }

    fn stream<'a>(
        &'a self,
        request: ModelRequest,
    ) -> ProviderFuture<'a, Result<StreamEvent, ProviderError>> {
        Box::pin(self.wire.stream(self.info.wire_api, request))
    }

    /// What OpenAI serves, from the cache when it is current and from the
    /// vendor otherwise.
    ///
    /// This never fails and never comes back empty, which is the difference
    /// from a provider that can only be asked: a fetch that fails falls through
    /// to the last answer received, and then to the compiled-in catalog. A
    /// person offline, behind a proxy, or not yet signed in still gets a
    /// picker — and one that lists a model the endpoint will honour, because
    /// the compiled-in list is the vendor's own.
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
                Ok(_) => Ok(fallback(cached)),
                Err(error) => {
                    tracing::debug!(%error, "could not list OpenAI's models; using what is on hand");
                    Ok(fallback(cached))
                }
            }
        })
    }
}

/// What to show when the vendor could not be asked: the last thing it said,
/// and failing that the catalog compiled in.
fn fallback(cached: Option<keke_catalog::Cached>) -> Vec<ModelInfo> {
    match cached {
        Some(cached) if !cached.models.is_empty() => cached.models,
        _ => catalog::bundled(),
    }
}

#[cfg(test)]
mod tests;
