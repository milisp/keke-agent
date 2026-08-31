//! The web-search seam.
//!
//! A hosted search handed to the model as a vendor tool entry is a capability
//! the model does not reliably see: it arrives with no description, in a tool
//! list otherwise full of the harness's own, and a coding agent reads its
//! surroundings and reaches for the shell instead. Worse, when the vendor does
//! run it the search happens inside the model call, where no approval reviewer
//! and no `ToolGuard` can see it and nothing tool-shaped exists to log.
//!
//! So the model is offered an ordinary tool instead — named, described, and
//! dispatched like any other — and this trait is what a provider implements to
//! answer it. The tool is neutral and the same on every vendor; only the fetch
//! behind it is the vendor's, which is what keeps a capability that only one
//! vendor has today from becoming a capability only one vendor can ever have.

use std::sync::Arc;

use crate::ProviderError;
use crate::ProviderFuture;

/// One source the search turned up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebSearchCitation {
    pub url: String,
    /// The page's title when the vendor reported one. Vendors differ on
    /// whether they do, and a URL alone is still a usable citation.
    pub title: Option<String>,
}

/// What a search came back with.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WebSearchResults {
    /// The prose answer, already summarized by whatever ran the search.
    ///
    /// Summary rather than raw pages because the alternative is spending the
    /// turn's whole context budget on markup: every vendor that offers hosted
    /// search offers it in this shape, and a harness that wanted the pages
    /// would be writing a crawler, not calling a provider.
    pub summary: String,
    /// Where the summary came from, deduplicated and in the order the vendor
    /// reported them.
    pub citations: Vec<WebSearchCitation>,
}

/// What a provider implements to answer the neutral `web_search` tool.
///
/// Implementers run the search however their vendor offers it and return the
/// result. They must not consult the conversation: a search is a function of
/// its query, and a backend that reached for turn state would be a second
/// model call the engine never logged.
pub trait WebSearchBackend: Send + Sync + 'static {
    /// Run one search.
    ///
    /// `allowed_domains` is the model's own request, which a deployment's
    /// configured policy may narrow but must never be widened by — a backend
    /// that let the model name a domain its configuration excluded would make
    /// the exclusion advisory.
    fn search<'a>(
        &'a self,
        query: &'a str,
        allowed_domains: &'a [String],
    ) -> ProviderFuture<'a, Result<WebSearchResults, ProviderError>>;
}

/// A backend as the composition root passes it around.
pub type ArcWebSearch = Arc<dyn WebSearchBackend>;
