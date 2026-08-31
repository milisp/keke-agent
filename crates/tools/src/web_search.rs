use std::sync::Arc;

use keke_protocol::ContentBlock;
use keke_provider_api::ArcWebSearch;
use keke_tool::ListToolsContext;
use keke_tool::Tool;
use keke_tool::ToolCallContext;
use keke_tool::ToolCapabilities;
use keke_tool::ToolDescription;
use keke_tool::ToolError;
use keke_tool::ToolId;
use keke_tool::ToolKind;
use keke_tool::ToolOutput;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WebSearchArgs {
    /// What to search for, as a natural-language query.
    pub query: String,
    /// Restrict the search to these domains, e.g. `["docs.rs"]`.
    #[serde(default)]
    pub allowed_domains: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct WebSearchOutput {
    pub query: String,
    pub summary: String,
    pub citations: Vec<Citation>,
}

#[derive(Debug, Serialize)]
pub struct Citation {
    pub url: String,
    pub title: Option<String>,
}

impl ToolOutput for WebSearchOutput {
    fn render(&self) -> Vec<ContentBlock> {
        let mut text = self.summary.clone();
        if !self.citations.is_empty() {
            text.push_str("\n\nSources:\n");
            for citation in &self.citations {
                match &citation.title {
                    Some(title) => text.push_str(&format!("- {title} — {}\n", citation.url)),
                    None => text.push_str(&format!("- {}\n", citation.url)),
                }
            }
        }
        vec![ContentBlock::text(text)]
    }
}

/// Searches the web through whatever the session's provider offers.
///
/// The tool is neutral and its backend is not: what the model sees is one
/// named, described tool that behaves the same on every vendor, which is the
/// whole point. A vendor's hosted search handed to the model as a bare tool
/// entry is a capability the model does not reliably notice — and one that,
/// running inside the model call, no approval reviewer and no `ToolGuard` can
/// see and nothing tool-shaped records. Dispatched here it is an ordinary
/// call: reviewed like one, logged like one.
pub struct WebSearch {
    backend: ArcWebSearch,
}

impl WebSearch {
    #[must_use]
    pub fn new(backend: ArcWebSearch) -> Self {
        Self { backend }
    }
}

impl Tool for WebSearch {
    type Args = WebSearchArgs;
    type Output = WebSearchOutput;

    fn id(&self) -> ToolId {
        ToolId::new("web_search")
    }

    fn description(&self, _ctx: &ListToolsContext) -> ToolDescription {
        // Says "current" and "outside this workspace" because the failure
        // this tool exists to fix was a model in a repository reading a
        // question about the world as a question about the checkout, and
        // reaching for `grep` and then `curl`.
        ToolDescription::new(
            "Search the web for current information from outside this workspace — releases, \
             documentation, announcements, error messages, anything published after the model's \
             training cut-off. Returns a summary with source URLs. Prefer this over shelling out \
             to `curl` for anything on the public web.",
        )
    }

    fn capabilities(&self) -> ToolCapabilities {
        // `Network`, not `Search`: the argument reaches a third party and the
        // reply comes back into the turn's context, so this is not the
        // read-only local lookup `grep` is and must not be grouped with one.
        ToolCapabilities::of_kind(ToolKind::Network)
    }

    async fn run(
        &self,
        _ctx: ToolCallContext,
        args: Self::Args,
    ) -> Result<Self::Output, ToolError> {
        if args.query.trim().is_empty() {
            return Err(ToolError::InvalidArgs {
                tool: "web_search".to_string(),
                message: "`query` must not be empty".to_string(),
            });
        }
        let results = self
            .backend
            .search(&args.query, &args.allowed_domains)
            .await
            .map_err(|error| ToolError::custom("search_failed", error.to_string()))?;
        Ok(WebSearchOutput {
            query: args.query,
            summary: results.summary,
            citations: results
                .citations
                .into_iter()
                .map(|citation| Citation {
                    url: citation.url,
                    title: citation.title,
                })
                .collect(),
        })
    }
}

struct WebSearchTool(ArcWebSearch);

impl keke_plugin_api::ToolContributor for WebSearchTool {
    fn tools(&self, _ctx: &keke_plugin_api::ExtensionContext) -> Vec<keke_tool::ArcTool> {
        vec![Arc::new(WebSearch::new(Arc::clone(&self.0)))]
    }
}

/// Register the `web_search` tool, backed by `backend`.
///
/// Separate from [`install`](crate::install) because the rest of the pack is
/// unconditional and this is not: a session whose provider offers no search
/// registers nothing here and advertises no tool.
pub fn install_web_search(
    registry: &mut keke_plugin_api::ExtensionRegistryBuilder,
    backend: ArcWebSearch,
) {
    registry.tool_contributor(Arc::new(WebSearchTool(backend)));
}
