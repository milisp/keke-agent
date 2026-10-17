//! Anthropic's server-side web search, translated from the neutral
//! [`WebSearchConfig`].
//!
//! Anthropic offers search as a server tool on its messages wire: the model
//! issues the queries, the vendor runs them, and the reply carries the answer
//! as text blocks whose `citations` point at the pages read. See
//! <https://docs.claude.com/en/docs/agents-and-tools/tool-use/web-search-tool>.
//!
//! [`WebSearchConfig::include_images`] has no counterpart here and is dropped
//! rather than refused: it asks for *more* than this search returns, so
//! ignoring it under-delivers where ignoring a restriction would over-grant.

use keke_config_types::WebSearchConfig;
use keke_config_types::WebSearchContextSize;
use keke_config_types::WebSearchMode;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;

/// The server tool's version.
///
/// The basic variant rather than the dynamic-filtering one: it is the one
/// every search-capable model accepts and the only one some platforms serve,
/// and dynamic filtering runs code execution under the hood, which is a
/// second billed tool this call has no use for — it wants a summary, not a
/// filtered corpus.
const TOOL_TYPE: &str = "web_search_20250305";

/// How many searches each context size buys.
///
/// Anthropic counts searches where OpenAI sizes a context budget, so the
/// ladder is mapped rather than passed through. Not a constant in the sense
/// invariant 9 forbids: the knob a deployment turns is
/// `web_search.context_size`, and this is only what this vendor calls it.
const fn max_uses(size: WebSearchContextSize) -> u32 {
    match size {
        WebSearchContextSize::Low => 1,
        WebSearchContextSize::Medium => 3,
        WebSearchContextSize::High => 5,
    }
}

/// A reply budget for the summary. Thinking on current models counts against
/// it, so it is sized for that rather than for the few paragraphs asked for.
const MAX_TOKENS: u32 = 16_000;

/// Whether `mode` offers a search at all, refusing what this vendor cannot
/// honor.
///
/// [`WebSearchMode::Cached`] and [`WebSearchMode::Indexed`] are refused rather
/// than approximated: Anthropic's search exposes no cached-only or
/// index-confined tier, so honoring either as best-effort would hand live
/// outbound access to precisely the deployment that wrote down it may not have
/// it — invariant 8's case exactly.
pub(crate) fn offered(mode: WebSearchMode) -> Result<bool, String> {
    match mode {
        WebSearchMode::Disabled => Ok(false),
        WebSearchMode::Live => Ok(true),
        WebSearchMode::Cached | WebSearchMode::Indexed => Err(format!(
            "Anthropic's web search has no cached or index-only tier, so web_search.mode `{}` \
             cannot be honored here; use `live` to accept live searches or `disabled` to offer \
             no search",
            mode.as_str()
        )),
    }
}

/// The server tool entry for one search.
fn tool(config: &WebSearchConfig, domains: &[String]) -> Value {
    let mut spec = Map::new();
    spec.insert("type".to_string(), json!(TOOL_TYPE));
    spec.insert("name".to_string(), json!("web_search"));
    spec.insert("max_uses".to_string(), json!(max_uses(config.context_size)));
    if !domains.is_empty() {
        spec.insert("allowed_domains".to_string(), json!(domains));
    }
    if let Some(location) = config.user_location.as_ref().filter(|l| !l.is_empty()) {
        let mut wire = Map::new();
        // The only kind this API takes, and a location without it is refused.
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
        spec.insert("user_location".to_string(), Value::Object(wire));
    }
    Value::Object(spec)
}

/// One search as a self-contained messages request.
///
/// `tool_choice` is left at `auto` and the instruction to search goes in the
/// system prompt instead: current models refuse a forced tool choice outright,
/// and a request that works on every model is worth more than one that is
/// strict on some.
pub(crate) fn request(
    config: &WebSearchConfig,
    model: &str,
    query: &str,
    domains: &[String],
) -> Value {
    json!({
        "model": model,
        "max_tokens": MAX_TOKENS,
        "system": "You run web searches for a coding agent. Search the web for the user's query \
                   — do not answer from memory — and reply with a concise, factual summary of \
                   what the sources say, citing them.",
        "messages": [{"role": "user", "content": query}],
        "tools": [tool(config, domains)],
    })
}

/// What one reply contributed.
#[derive(Debug, Default)]
pub(crate) struct Reading {
    /// The text after the last search result, which is the answer; anything
    /// before it is the model announcing what it is about to look up.
    pub summary: String,
    /// Sources cited in the answer, in order, possibly repeated.
    pub cited: Vec<(String, Option<String>)>,
    /// Every page the searches returned, for an answer that cited none.
    pub found: Vec<(String, Option<String>)>,
    /// The vendor's error code, when a search failed. Server-tool errors are
    /// not HTTP errors: they arrive as a successful reply whose result block
    /// holds an object instead of a list.
    pub error: Option<String>,
}

fn url_and_title(value: &Value) -> Option<(String, Option<String>)> {
    let url = value.get("url").and_then(Value::as_str)?;
    let title = value
        .get("title")
        .and_then(Value::as_str)
        .filter(|title| !title.is_empty())
        .map(str::to_string);
    Some((url.to_string(), title))
}

/// Read one reply's content blocks into `reading`.
pub(crate) fn read(body: &Value, reading: &mut Reading) {
    let blocks = body.get("content").and_then(Value::as_array);
    for block in blocks.into_iter().flatten() {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    reading.summary.push_str(text);
                }
                for citation in block
                    .get("citations")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if let Some(source) = url_and_title(citation) {
                        reading.cited.push(source);
                    }
                }
            }
            Some("web_search_tool_result") => {
                reading.summary.clear();
                match block.get("content") {
                    Some(Value::Array(results)) => {
                        reading
                            .found
                            .extend(results.iter().filter_map(url_and_title));
                    }
                    Some(error) => {
                        reading.error = error
                            .get("error_code")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                    }
                    None => {}
                }
            }
            _ => {}
        }
    }
}
