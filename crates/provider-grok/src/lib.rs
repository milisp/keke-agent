//! The xAI Grok model provider.
//!
//! xAI serves the OpenAI schemas, so everything about the translation and the
//! stream is shared with every other vendor that does — [`keke_wire`] owns it.
//! What is left here, and all that should be here, is the vendor's identity:
//! where it lives, what credential it uses, which wire format it speaks, and
//! the shape of the catalog it publishes.
//!
//! One vendor, two addresses. A grok *login* is spent at the subscription
//! proxy, which speaks the responses wire, fixes its own sampling, and
//! publishes a catalog with reasoning levels in it. An API key is spent at the
//! pay-per-token API, which takes chat completions and answers `/models` with
//! the plain listing. Both can be registered at once, under names a deployment
//! chooses, because an [`Endpoint`] carries its own route rather than taking
//! [`ROUTE`] as given — so the address, the wire, and the account all arrive
//! from the composition root rather than being guessed here.

mod catalog;
mod web_search;

use std::sync::Arc;

use keke_auth_api::AuthProvider;
use keke_catalog::CatalogCache;
use keke_config_types::WebSearchConfig;
use keke_provider_api::ModelInfo;
use keke_provider_api::ModelProvider;
use keke_provider_api::ModelRequest;
use keke_provider_api::ProviderError;
use keke_provider_api::ProviderFuture;
use keke_provider_api::ProviderInfo;
use keke_provider_api::StreamEvent;
use keke_provider_api::WireApi;
use keke_wire::WireClient;
use serde_json::Value;

/// The route key this provider registers under, and the auth id it draws its
/// credentials from.
pub const ROUTE: &str = "grok";

/// The public xAI endpoint, where an API key is spent.
pub const DEFAULT_BASE_URL: &str = "https://api.x.ai/v1";

/// How to reach xAI for one session.
pub struct Endpoint {
    /// What this instance registers as. [`ROUTE`] is the default name, not the
    /// only one: xAI's two addresses are two instances of this provider, and a
    /// deployment may run both — so the registry key is an argument rather
    /// than a constant baked in here.
    pub route: String,
    /// Shown in surfaces. Two instances of one vendor need telling apart.
    pub display_name: String,
    /// Which registered [`keke_auth_api::AuthProvider`] this instance draws on.
    /// [`ROUTE`] for the account in force; `route/account` for a named one.
    /// Supplied rather than derived because only the composition root knows
    /// which accounts were registered.
    pub auth_id: String,
    pub base_url: String,
    pub wire_api: WireApi,
    /// Whether this address refuses a request that names a reply budget or a
    /// temperature, as the subscription proxy does.
    pub fixed_sampling: bool,
    /// Whether this instance offers xAI's own web search, and on what terms.
    /// Defaults to offering none: the search runs at the vendor, inside the
    /// model call, where neither the approval seam nor a `ToolGuard` can see
    /// it — so a person turns it on rather than discovering it is on.
    pub web_search: WebSearchConfig,
}

impl Default for Endpoint {
    /// The pay-per-token API, which is what an API key reaches.
    fn default() -> Self {
        Self {
            route: ROUTE.to_string(),
            display_name: "Grok".to_string(),
            auth_id: ROUTE.to_string(),
            base_url: DEFAULT_BASE_URL.to_string(),
            wire_api: WireApi::ChatCompletions,
            fixed_sampling: false,
            web_search: WebSearchConfig::default(),
        }
    }
}

/// xAI's Grok models.
pub struct GrokProvider {
    info: ProviderInfo,
    wire: WireClient,
    /// `None` disables caching entirely, which is what a surface with no home
    /// directory — a test — gets.
    cache: Option<CatalogCache>,
    /// The hosted search this instance asks for, built once because it is the
    /// same on every request, and in the shape this endpoint's wire wants:
    /// a `search_parameters` field on chat-completions, a tool entry on
    /// responses. `None` when the deployment offers none.
    search: Option<Value>,
}

impl GrokProvider {
    /// # Errors
    ///
    /// [`ProviderError::InvalidRequest`] when `endpoint.web_search` asks for
    /// access xAI cannot express — see [`web_search`]. Refused at construction
    /// rather than per turn, so a deployment learns from its first launch and
    /// not from its bill.
    pub fn new(
        auth: Arc<dyn AuthProvider>,
        endpoint: Endpoint,
        cache: Option<CatalogCache>,
    ) -> Result<Self, ProviderError> {
        let search = match endpoint.wire_api {
            WireApi::Responses => web_search::tool(&endpoint.web_search),
            _ => web_search::parameters(&endpoint.web_search),
        }
        .map_err(ProviderError::InvalidRequest)?;
        let base_url = if endpoint.base_url.trim().is_empty() {
            DEFAULT_BASE_URL.to_string()
        } else {
            endpoint.base_url
        };
        let mut wire = WireClient::new(base_url, auth);
        if endpoint.fixed_sampling {
            wire = wire.with_fixed_sampling();
        }
        Ok(Self {
            info: ProviderInfo {
                route: endpoint.route,
                display_name: endpoint.display_name,
                base_url: wire.base_url().to_string(),
                wire_api: endpoint.wire_api,
                auth_id: Some(endpoint.auth_id),
                env_key: Some("XAI_API_KEY".to_string()),
            },
            wire,
            cache,
            search,
        })
    }

    async fn fetch(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let body = self.wire.fetch("/models").await?;
        catalog::parse(&body)
            .map_err(|error| ProviderError::Protocol(format!("undecodable model list: {error}")))
    }
}

impl ModelProvider for GrokProvider {
    fn info(&self) -> &ProviderInfo {
        &self.info
    }

    /// The hosted search is attached here rather than by the engine, because
    /// the engine is not allowed to know that this vendor has one — and because
    /// it is a property of the endpoint, not of the turn.
    fn stream<'a>(
        &'a self,
        mut request: ModelRequest,
    ) -> ProviderFuture<'a, Result<StreamEvent, ProviderError>> {
        if let Some(search) = &self.search {
            match self.info.wire_api {
                WireApi::Responses => request.hosted_tools.push(search.clone()),
                _ => {
                    request
                        .vendor_params
                        .insert("search_parameters".to_string(), search.clone());
                }
            }
        }
        Box::pin(self.wire.stream(self.info.wire_api, request))
    }

    /// What xAI serves, from the cache when it is current and from the vendor
    /// otherwise.
    ///
    /// This never fails and never comes back empty. A fetch that fails falls
    /// through to the last answer received, and then to the compiled-in
    /// catalog: a person offline or not yet signed in still gets a picker, and
    /// one that lists models the endpoint will honour because the compiled-in
    /// list is the vendor's own.
    fn list_models(&self) -> ProviderFuture<'_, Result<Vec<ModelInfo>, ProviderError>> {
        Box::pin(async move {
            // Keyed by this instance's route, not by the vendor: the two
            // addresses publish different catalogs, and one key for both would
            // have each overwrite the other's listing every time the picker
            // opened.
            let route = self.info.route.as_str();
            let cached = self.cache.as_ref().and_then(|cache| cache.load(route));
            if let Some(cached) = &cached
                && cached.fresh
            {
                return Ok(cached.models.clone());
            }
            match self.fetch().await {
                Ok(models) if !models.is_empty() => {
                    if let Some(cache) = &self.cache {
                        cache.store(route, &models);
                    }
                    Ok(models)
                }
                Ok(_) => Ok(fallback(cached)),
                Err(error) => {
                    tracing::debug!(%error, "could not list xAI's models; using what is on hand");
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
