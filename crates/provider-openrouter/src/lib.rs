//! OpenRouter's web search, behind keke's neutral `web_search` tool.
//!
//! OpenRouter's conversation route needs no crate of its own: it is a declared
//! OpenAI-compatible endpoint. What does not fit there is the search, which
//! OpenRouter offers as its `openrouter:web_search` server tool — any model,
//! with OpenRouter choosing the engine (the model vendor's own search where it
//! has one, Exa otherwise). See
//! <https://openrouter.ai/docs/guides/features/server-tools/web-search>.
//!
//! [`WebSearchConfig::include_images`] has no counterpart here and is dropped
//! rather than refused: it asks for *more* than this search returns, so
//! ignoring it under-delivers where ignoring a restriction would over-grant.

use keke_config_types::WebSearchConfig;
use keke_config_types::WebSearchContextSize;
use keke_config_types::WebSearchMode;
use keke_provider_api::ProviderError;
use keke_provider_api::ProviderFuture;
use keke_provider_api::WebSearchBackend;
use keke_provider_api::WebSearchCitation;
use keke_provider_api::WebSearchResults;
use keke_wire::WireClient;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;

/// How many searches each context size buys.
///
/// OpenRouter takes `search_context_size` too, but its engines read it as a
/// per-result character budget, and the native ones ignore it — so the number
/// of searches is what bounds the bill everywhere. Not a constant in the sense
/// invariant 9 forbids: the knob a deployment turns is
/// `web_search.context_size`, and this is only what this vendor calls it.
const fn max_uses(size: WebSearchContextSize) -> u32 {
    match size {
        WebSearchContextSize::Low => 1,
        WebSearchContextSize::Medium => 3,
        WebSearchContextSize::High => 5,
    }
}

/// OpenRouter's search, run as a call of its own.
///
/// The conversation's request is left alone: the model sees one described
/// `web_search` tool, and calling it sends a separate, non-streaming
/// chat-completions request carrying OpenRouter's server tool and nothing
/// else. Run from inside the conversation instead, the search would happen
/// where no approval reviewer and no `ToolGuard` can see it.
pub struct OpenRouterWebSearch {
    wire: WireClient,
    config: WebSearchConfig,
    model: String,
}

impl OpenRouterWebSearch {
    /// The search `config` asks for, or `None` when it asks for none.
    ///
    /// `wire` is the route's own client — same address, credential, proxy,
    /// and attribution headers — rather than one built here, so a search does
    /// not reach OpenRouter by a path the conversation could not.
    ///
    /// # Errors
    ///
    /// [`ProviderError::InvalidRequest`] when `config.mode` asks for access
    /// OpenRouter cannot express, or when no model is named to run the
    /// search. OpenRouter lists hundreds of models in no useful order, so
    /// unlike a single vendor's listing there is no first one to fall back
    /// on. Both are refused at construction, so a deployment learns from its
    /// first launch and not from its first search.
    pub fn new(
        wire: WireClient,
        config: WebSearchConfig,
        default_model: Option<String>,
    ) -> Result<Option<Self>, ProviderError> {
        match config.mode {
            WebSearchMode::Disabled => return Ok(None),
            WebSearchMode::Live => {}
            // OpenRouter's engines search live and expose no cached-only or
            // index-confined tier, so honoring either as best-effort would
            // hand live outbound access to the deployment that wrote down it
            // may not have it.
            mode @ (WebSearchMode::Cached | WebSearchMode::Indexed) => {
                return Err(ProviderError::InvalidRequest(format!(
                    "OpenRouter's web search has no cached or index-only tier, so \
                     web_search.mode `{}` cannot be honored here; use `live` to accept live \
                     searches or `disabled` to offer no search",
                    mode.as_str()
                )));
            }
        }
        let model = [config.model.as_deref(), default_model.as_deref()]
            .into_iter()
            .flatten()
            .map(str::trim)
            .find(|model| !model.is_empty())
            .map(str::to_string)
            .ok_or_else(|| {
                ProviderError::InvalidRequest(
                    "no model to run the search with: set `web_search.model` or the route's \
                     `default_model`"
                        .to_string(),
                )
            })?;
        Ok(Some(Self {
            wire,
            config,
            model,
        }))
    }

    fn tool(&self, domains: &[String]) -> Value {
        let mut parameters = Map::new();
        parameters.insert(
            "max_uses".to_string(),
            json!(max_uses(self.config.context_size)),
        );
        parameters.insert(
            "search_context_size".to_string(),
            json!(self.config.context_size.as_str()),
        );
        if !domains.is_empty() {
            parameters.insert("allowed_domains".to_string(), json!(domains));
        }
        if let Some(location) = self.config.user_location.as_ref().filter(|l| !l.is_empty()) {
            let mut wire = Map::new();
            wire.insert("type".to_string(), json!("approximate"));
            for (field, value) in [
                ("country", &location.country),
                ("region", &location.region),
                ("city", &location.city),
                ("timezone", &location.timezone),
            ] {
                if let Some(value) = value {
                    wire.insert(field.to_string(), json!(value));
                }
            }
            parameters.insert("user_location".to_string(), Value::Object(wire));
        }
        json!({"type": "openrouter:web_search", "parameters": parameters})
    }

    /// One search as a self-contained request.
    ///
    /// `tool_choice` is left at `auto` and the instruction to search goes in
    /// the system prompt: the model behind the route may be any vendor's, and
    /// several current ones refuse a forced tool choice outright.
    fn request(&self, query: &str, domains: &[String]) -> Value {
        json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": "You run web searches for a coding agent. Search \
                    the web for the user's query — do not answer from memory — and reply with a \
                    concise, factual summary of what the sources say, citing them."},
                {"role": "user", "content": query},
            ],
            "tools": [self.tool(domains)],
            "stream": false,
        })
    }
}

/// The summary and sources in a chat-completions reply.
///
/// Sources arrive as `url_citation` annotations on the message. The URL sits
/// in a nested `url_citation` object in OpenRouter's documented shape and at
/// the top level in some engines' replies, so both are read.
fn read(body: &Value) -> (String, Vec<(String, Option<String>)>) {
    let message = body
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"));
    let summary = message
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let citations = message
        .and_then(|message| message.get("annotations"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|annotation| {
            let source = annotation.get("url_citation").unwrap_or(annotation);
            let url = source.get("url").and_then(Value::as_str)?;
            let title = source
                .get("title")
                .and_then(Value::as_str)
                .filter(|title| !title.is_empty())
                .map(str::to_string);
            Some((url.to_string(), title))
        })
        .collect();
    (summary, citations)
}

impl WebSearchBackend for OpenRouterWebSearch {
    fn search<'a>(
        &'a self,
        query: &'a str,
        allowed_domains: &'a [String],
    ) -> ProviderFuture<'a, Result<WebSearchResults, ProviderError>> {
        Box::pin(async move {
            let domains = self
                .config
                .confine(allowed_domains)
                .map_err(ProviderError::InvalidRequest)?;
            let raw = self
                .wire
                .post("/chat/completions", &self.request(query, &domains))
                .await?;
            // Trace rather than debug: the reply is a person's query and its
            // answers, and belongs in a log only when someone has turned one
            // on to look at this.
            tracing::trace!(%raw, "OpenRouter answered the search");
            let reply: Value = serde_json::from_str(&raw)
                .map_err(|error| ProviderError::Protocol(format!("undecodable search: {error}")))?;
            let (summary, citations) = read(&reply);
            // An empty answer is reported as one rather than as a successful
            // search of nothing: a model told "here are your results" with no
            // results writes as if it had them.
            if summary.is_empty() && citations.is_empty() {
                return Err(ProviderError::Protocol(
                    "the search returned no results and no sources".to_string(),
                ));
            }
            let mut seen = std::collections::BTreeSet::new();
            Ok(WebSearchResults {
                summary,
                citations: citations
                    .into_iter()
                    .filter(|(url, _)| seen.insert(url.clone()))
                    .map(|(url, title)| WebSearchCitation { url, title })
                    .collect(),
            })
        })
    }
}

#[cfg(test)]
mod tests;
