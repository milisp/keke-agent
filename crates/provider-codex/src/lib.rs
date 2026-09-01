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
use keke_provider_api::ServiceTier;
use keke_provider_api::StreamEvent;
use keke_provider_api::WireApi;
use keke_wire::WireClient;

/// The route key this provider registers under, and the auth id it draws its
/// credentials from.
pub const ROUTE: &str = "codex";

/// The client version `/models` is gated on.
///
/// Not keke's version, and the difference is not cosmetic. The subscription
/// backend requires a `client_version` query parameter — a request without one
/// is refused as
/// `invalid request: [{'type': 'missing', 'loc': ('query', 'client_version')}]`
/// — and then gates each model on it, answering `{"models":[]}` for a client it
/// considers too old. Both failures read as "this vendor publishes no catalog",
/// which sends the listing to the compiled-in fallback *without* filling the
/// cache, so every launch pays for a round trip whose result is thrown away.
///
/// Upstream sends its own crate version (`codex-models-manager`'s
/// `client_version_to_whole()`), and codex versions calendrically — its
/// app-server gates features on `version.starts_with("26.4")`. Measured against
/// the live endpoint, that shape is what the gate wants: `0.1.10` (keke's own
/// version at the time) returns nothing, `0.99.0` returns two of six models, and
/// `26.4.0` returns all six. Sending keke's version here would therefore buy an
/// empty catalog on every launch, so this tracks the protocol generation the
/// vendor expects instead — and a deployment that meets a raised gate before
/// keke ships a new release needs to say so without forking the plugin.
pub const DEFAULT_CLIENT_VERSION: &str = "26.4.0";

/// OpenAI over the responses wire.
pub struct CodexProvider {
    info: ProviderInfo,
    wire: WireClient,
    /// `None` disables caching entirely, which is what a surface with no home
    /// directory — a test — gets.
    cache: Option<CatalogCache>,
    client_version: String,
    /// The hosted search tool this instance advertises, built once because it
    /// is the same on every request. `None` when the deployment offers none.
    web_search: Option<serde_json::Value>,
}

/// How to reach OpenAI for one session.
pub struct Endpoint {
    /// What this instance registers as. [`ROUTE`] is the default name, not the
    /// only one: OpenAI's two addresses are two instances of this provider,
    /// and a deployment may run both.
    pub route: String,
    /// Shown in surfaces. Two instances of one vendor need telling apart.
    pub display_name: String,
    /// Which registered [`keke_auth_api::AuthProvider`] this instance draws on.
    /// [`ROUTE`] for the account in force; `route/account` for a named one.
    /// Supplied rather than derived because only the composition root knows
    /// which accounts were registered.
    pub auth_id: String,
    /// Where to send requests. The composition root picks it from the stored
    /// credential, because a subscription token at the public API and an API
    /// key at the subscription backend both fail as a 401 that names neither
    /// the address nor the account.
    pub base_url: String,
    /// Whether this address refuses a request that names a reply budget or a
    /// temperature, as the subscription backend does.
    pub fixed_sampling: bool,
    /// Sent as the `client_version` query parameter on `/models` — see
    /// [`DEFAULT_CLIENT_VERSION`].
    pub client_version: String,
    /// Whether this instance offers OpenAI's hosted web search, and on what
    /// terms. Defaults to offering none: the search runs at the vendor, inside
    /// the model call, where neither the approval seam nor a `ToolGuard` can
    /// see it — so a person turns it on rather than discovering it is on.
    pub web_search: WebSearchConfig,
}

impl Default for Endpoint {
    /// The ChatGPT backend under this provider's own name. The address is not
    /// defaulted to the public API because this provider exists for the
    /// subscription surface; a key-only instance states its address.
    fn default() -> Self {
        Self {
            route: ROUTE.to_string(),
            display_name: "ChatGPT".to_string(),
            auth_id: ROUTE.to_string(),
            base_url: "https://chatgpt.com/backend-api/codex".to_string(),
            fixed_sampling: true,
            client_version: DEFAULT_CLIENT_VERSION.to_string(),
            web_search: WebSearchConfig::default(),
        }
    }
}

impl CodexProvider {
    #[must_use]
    pub fn new(
        auth: Arc<dyn AuthProvider>,
        endpoint: Endpoint,
        cache: Option<CatalogCache>,
    ) -> Self {
        let mut wire = WireClient::new(endpoint.base_url.clone(), auth);
        if endpoint.fixed_sampling {
            wire = wire.with_fixed_sampling();
        }
        let client_version = endpoint.client_version;
        let web_search = web_search::tool(&endpoint.web_search);
        Self {
            info: ProviderInfo {
                route: endpoint.route,
                display_name: endpoint.display_name,
                base_url: wire.base_url().to_string(),
                wire_api: WireApi::Responses,
                auth_id: Some(endpoint.auth_id),
                env_key: Some("OPENAI_API_KEY".to_string()),
            },
            wire,
            cache,
            client_version,
            web_search,
        }
    }

    /// Ask the endpoint what it serves.
    async fn fetch(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let body = self
            .wire
            .fetch(&format!("/models?client_version={}", self.client_version))
            .await?;
        catalog::parse(&body)
            .map_err(|error| ProviderError::Protocol(format!("undecodable model list: {error}")))
    }
}

impl ModelProvider for CodexProvider {
    fn info(&self) -> &ProviderInfo {
        &self.info
    }

    /// The hosted search is attached here rather than by the engine, because
    /// the engine is not allowed to know that this vendor has one — and because
    /// it is a property of the endpoint, not of the turn.
    ///
    /// The turn's service tier is translated here for the neighbouring reason:
    /// the engine names a queue in keke's own words, and `priority` is this
    /// vendor's word for the fast one. It is sent as written rather than
    /// checked against the model — a queue this account cannot buy is refused
    /// by the endpoint, naming itself, where a check here could only guess from
    /// a catalog that may be the compiled-in fallback.
    fn stream<'a>(
        &'a self,
        mut request: ModelRequest,
    ) -> ProviderFuture<'a, Result<StreamEvent, ProviderError>> {
        if let Some(tool) = &self.web_search {
            request.hosted_tools.push(tool.clone());
        }
        if let Some(tier) = request.service_tier {
            request.vendor_params.insert(
                "service_tier".to_string(),
                serde_json::json!(wire_tier(tier)),
            );
        }
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
            // Keyed by this instance's route, not by the vendor: two
            // instances serve different addresses, and one key for both would
            // have each overwrite the other's listing.
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
                    tracing::debug!(%error, "could not list OpenAI's models; using what is on hand");
                    Ok(fallback(cached))
                }
            }
        })
    }
}

/// OpenAI's own name for a queue. `Fast` is `priority` here because that is
/// what this vendor calls it; the word a person types stays keke's.
fn wire_tier(tier: ServiceTier) -> &'static str {
    match tier {
        ServiceTier::Fast => "priority",
        ServiceTier::Flex => "flex",
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
