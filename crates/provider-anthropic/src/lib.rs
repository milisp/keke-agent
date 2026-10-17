//! Anthropic's web search, behind keke's neutral `web_search` tool.
//!
//! Anthropic's conversation route needs no crate of its own: it is the shared
//! messages wire at an address with a key, and the composition root builds it
//! from `keke-wire` directly. What does not fit there is the search, which is
//! a second request of a vendor-specific shape — so this crate holds that and
//! nothing else.

mod web_search;

use std::sync::Arc;

use keke_auth_api::AuthProvider;
use keke_config_types::WebSearchConfig;
use keke_provider_api::ProviderError;
use keke_provider_api::ProviderFuture;
use keke_provider_api::WebSearchBackend;
use keke_provider_api::WebSearchCitation;
use keke_provider_api::WebSearchResults;
use keke_provider_api::WireApi;
use keke_wire::WireClient;
use serde_json::Value;
use serde_json::json;

/// How many times a search the vendor paused is resumed before what it has
/// so far is taken as the answer.
///
/// The server runs its own tool loop and hands the turn back with
/// `pause_turn` when that loop runs long; resuming is the documented reply. A
/// bound, because a search is a tool call inside someone's turn and a loop
/// with none would hold that turn for as long as the vendor kept pausing.
const MAX_RESUMES: usize = 3;

/// Anthropic's search, run as a call of its own.
///
/// The conversation's request is left alone: the model sees one described
/// `web_search` tool, and calling it sends a separate, non-streaming messages
/// request carrying Anthropic's server tool and nothing else. Run from inside
/// the conversation instead, the search would happen where no approval
/// reviewer and no `ToolGuard` can see it.
pub struct AnthropicWebSearch {
    wire: WireClient,
    config: WebSearchConfig,
    default_model: Option<String>,
}

impl AnthropicWebSearch {
    /// The search `config` asks for, or `None` when it asks for none.
    ///
    /// # Errors
    ///
    /// [`ProviderError::InvalidRequest`] when `config.mode` asks for access
    /// this vendor cannot express. Refused at construction rather than per
    /// call, so a deployment learns from its first launch and not from its
    /// bill.
    pub fn new(
        base_url: String,
        auth: Arc<dyn AuthProvider>,
        config: WebSearchConfig,
        default_model: Option<String>,
    ) -> Result<Option<Self>, ProviderError> {
        if !web_search::offered(config.mode).map_err(ProviderError::InvalidRequest)? {
            return Ok(None);
        }
        Ok(Some(Self {
            wire: WireClient::new(base_url, auth),
            config,
            default_model,
        }))
    }

    /// Which model runs the search.
    ///
    /// The deployment's `web_search.model`, else this route's default, else
    /// the first model Anthropic lists — which it lists newest first.
    async fn model(&self) -> Result<String, ProviderError> {
        let named = [self.config.model.as_deref(), self.default_model.as_deref()]
            .into_iter()
            .flatten()
            .map(str::trim)
            .find(|model| !model.is_empty());
        if let Some(model) = named {
            return Ok(model.to_string());
        }
        let listing: Value =
            serde_json::from_str(&self.wire.fetch_with(WireApi::Messages, "/models").await?)
                .map_err(|error| {
                    ProviderError::Protocol(format!("undecodable model list: {error}"))
                })?;
        listing
            .get("data")
            .and_then(Value::as_array)
            .and_then(|models| models.first())
            .and_then(|model| model.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| {
                ProviderError::InvalidRequest(
                    "no model to run the search with: set `web_search.model` or the route's \
                     `default_model`"
                        .to_string(),
                )
            })
    }
}

impl WebSearchBackend for AnthropicWebSearch {
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
            let model = self.model().await?;
            let mut body = web_search::request(&self.config, &model, query, &domains);
            let mut reading = web_search::Reading::default();

            for _ in 0..=MAX_RESUMES {
                let raw = self
                    .wire
                    .post_with(WireApi::Messages, "/messages", &body)
                    .await?;
                // Trace rather than debug: the reply is a person's query and
                // its answers, and belongs in a log only when someone has
                // turned one on to look at this.
                tracing::trace!(%raw, "Anthropic answered the search");
                let reply: Value = serde_json::from_str(&raw).map_err(|error| {
                    ProviderError::Protocol(format!("undecodable search: {error}"))
                })?;
                web_search::read(&reply, &mut reading);
                match reply.get("stop_reason").and_then(Value::as_str) {
                    Some("pause_turn") => {
                        // Resuming means handing the paused turn back as it
                        // was; the server picks up where it stopped.
                        let content = reply.get("content").cloned().unwrap_or(json!([]));
                        if let Some(messages) =
                            body.get_mut("messages").and_then(Value::as_array_mut)
                        {
                            messages.push(json!({"role": "assistant", "content": content}));
                        }
                    }
                    Some("refusal") => {
                        return Err(ProviderError::InvalidRequest(
                            "Anthropic declined to run this search".to_string(),
                        ));
                    }
                    _ => break,
                }
            }

            // An answer that cited nothing still came from somewhere, and the
            // pages the searches returned are the honest account of where.
            let sources = if reading.cited.is_empty() {
                reading.found
            } else {
                reading.cited
            };
            let summary = reading.summary.trim().to_string();
            // An empty answer is reported as one rather than as a successful
            // search of nothing: a model told "here are your results" with no
            // results writes as if it had them.
            if summary.is_empty() && sources.is_empty() {
                return Err(ProviderError::Protocol(match reading.error {
                    Some(code) => format!("Anthropic's web search failed: {code}"),
                    None => "the search returned no results and no sources".to_string(),
                }));
            }
            let mut seen = std::collections::BTreeSet::new();
            Ok(WebSearchResults {
                summary,
                citations: sources
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
