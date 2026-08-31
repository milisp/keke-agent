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
    /// What this route serves when nothing named a model, as the deployment
    /// declared it. Used only to decide which model runs a search when
    /// `web_search.model` is unset — the conversation's own model is chosen by
    /// the surface long after this, and a provider that guessed one here would
    /// be answering a question that was not asked of it.
    pub default_model: Option<String>,
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
            default_model: None,
        }
    }
}

/// xAI's Grok models.
pub struct GrokProvider {
    info: ProviderInfo,
    wire: Arc<WireClient>,
    /// `None` disables caching entirely, which is what a surface with no home
    /// directory — a test — gets.
    cache: Option<CatalogCache>,
    /// What answers the `web_search` tool, or `None` when the deployment
    /// offers no search.
    ///
    /// A backend rather than a blob spliced into every request: xAI's hosted
    /// search reaches the model as a bare `{"type":"web_search"}` entry with
    /// no description, which a coding agent in a repository does not read as
    /// "the way to reach the web" — it greps, then shells out to `curl`. Ran
    /// from a described tool instead, the search is a call the model can see,
    /// a guard can deny, and the rollout log records.
    search: Option<Arc<GrokWebSearch>>,
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
        // Refused here rather than per turn, by asking for the shape this
        // endpoint would send: a mode xAI cannot express is a launch failure,
        // not a bill.
        let offers_search = match endpoint.wire_api {
            WireApi::Responses => web_search::tool(&endpoint.web_search),
            _ => web_search::parameters(&endpoint.web_search),
        }
        .map_err(ProviderError::InvalidRequest)?
        .is_some();
        let base_url = if endpoint.base_url.trim().is_empty() {
            DEFAULT_BASE_URL.to_string()
        } else {
            endpoint.base_url
        };
        let mut wire = WireClient::new(base_url, auth);
        if endpoint.fixed_sampling {
            wire = wire.with_fixed_sampling();
        }
        let wire = Arc::new(wire);
        let search = offers_search.then(|| {
            Arc::new(GrokWebSearch {
                wire: Arc::clone(&wire),
                wire_api: endpoint.wire_api,
                config: endpoint.web_search.clone(),
                default_model: endpoint.default_model.clone(),
            })
        });
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

    /// Nothing about the search is spliced into the conversation's request.
    ///
    /// The hosted forms — a tool entry on responses, `search_parameters` on
    /// chat-completions — are both invisible to everything that reviews and
    /// records a turn, and the tool-entry form is invisible to the model too:
    /// it carries no description, and a coding agent reads its surroundings
    /// and shells out instead. So the capability is offered as an ordinary
    /// tool, and [`GrokWebSearch`] answers it in a call of its own.
    fn stream<'a>(
        &'a self,
        request: ModelRequest,
    ) -> ProviderFuture<'a, Result<StreamEvent, ProviderError>> {
        Box::pin(self.wire.stream(self.info.wire_api, request))
    }

    fn web_search(&self) -> Option<keke_provider_api::ArcWebSearch> {
        self.search
            .clone()
            .map(|search| search as keke_provider_api::ArcWebSearch)
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

/// xAI's search, run as a call of its own.
///
/// The shape is deliberately the one xAI's own CLI uses: a separate,
/// non-streaming request carrying the hosted search and nothing else, whose
/// prose answer and citations come back as a tool result. The conversation's
/// request is left alone, so the model's tool list holds one described
/// `web_search` and no bare vendor entry competing with it.
pub struct GrokWebSearch {
    wire: Arc<WireClient>,
    wire_api: WireApi,
    config: WebSearchConfig,
    default_model: Option<String>,
}

impl GrokWebSearch {
    /// Which model summarizes the results.
    ///
    /// A search is a self-contained call — query in, summary and sources out —
    /// so the deployment may name a cheaper model for it. Failing that this
    /// route's declared default, and failing that whatever xAI lists first,
    /// which is the same answer a surface with no model configured gets.
    async fn model(&self) -> Result<String, ProviderError> {
        if let Some(model) = self
            .config
            .model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
        {
            return Ok(model.to_string());
        }
        if let Some(model) = self
            .default_model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
        {
            return Ok(model.to_string());
        }
        let models = catalog::parse(&self.wire.fetch("/models").await?)
            .map_err(|error| ProviderError::Protocol(format!("undecodable model list: {error}")))?;
        models.first().map(|model| model.id.clone()).ok_or_else(|| {
            ProviderError::InvalidRequest(
                "no model to run the search with: set `web_search.model` or the route's \
                 `default_model`"
                    .to_string(),
            )
        })
    }
}

impl keke_provider_api::WebSearchBackend for GrokWebSearch {
    fn search<'a>(
        &'a self,
        query: &'a str,
        allowed_domains: &'a [String],
    ) -> ProviderFuture<'a, Result<keke_provider_api::WebSearchResults, ProviderError>> {
        Box::pin(async move {
            let domains = web_search::confine(&self.config, allowed_domains);
            let model = self.model().await?;
            // Each wire keeps this capability somewhere else — a tool entry on
            // responses, a top-level field on chat-completions — and reports
            // its sources somewhere else too, so both halves are per-wire
            // rather than one shape that would fit neither.
            let (path, body) = match self.wire_api {
                WireApi::Responses => (
                    "/responses",
                    web_search::responses_request(&model, query, &domains),
                ),
                _ => (
                    "/chat/completions",
                    web_search::chat_request(&self.config, &model, query, &domains),
                ),
            };
            let body = body.map_err(ProviderError::InvalidRequest)?;
            let raw = self.wire.post(path, &body).await?;
            // Trace rather than debug: the reply is the search's whole content,
            // which is a person's query and its answers, and belongs in a log
            // only when someone has turned one on to look at this.
            tracing::trace!(%raw, "xAI answered the search");
            let reply: Value = serde_json::from_str(&raw)
                .map_err(|error| ProviderError::Protocol(format!("undecodable search: {error}")))?;
            let (summary, citations) = match self.wire_api {
                WireApi::Responses => web_search::read_responses(&reply),
                _ => web_search::read_chat(&reply),
            };
            // An empty answer is reported as one rather than as a successful
            // search of nothing: a model told "here are your results" with no
            // results writes as if it had them.
            if summary.trim().is_empty() && citations.is_empty() {
                return Err(ProviderError::Protocol(
                    "the search returned no results and no sources".to_string(),
                ));
            }
            let mut seen = std::collections::BTreeSet::new();
            Ok(keke_provider_api::WebSearchResults {
                summary,
                citations: citations
                    .into_iter()
                    .filter(|(url, _)| seen.insert(url.clone()))
                    .map(|(url, title)| keke_provider_api::WebSearchCitation { url, title })
                    .collect(),
            })
        })
    }
}

#[cfg(test)]
mod tests;
