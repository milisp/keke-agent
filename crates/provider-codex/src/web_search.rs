//! OpenAI's hosted web search, as a `/responses` tool.
//!
//! The search runs inside the model call: the vendor issues the queries, reads
//! the pages, and folds what it found into the same turn. Nothing reaches the
//! harness, so this is the only place a deployment can say how far the search
//! may go — which is why the terms are a validated
//! [`keke_config_types::WebSearchConfig`] and not constants here.
//!
//! The two access flags are not one setting spelled twice.
//! `external_web_access` separates what the vendor already holds from what it
//! would go and fetch now; `indexed_web_access` then confines those live
//! fetches to URLs it has indexed. See
//! <https://platform.openai.com/docs/guides/tools-web-search>.

use keke_config_types::WebSearchConfig;
use keke_config_types::WebSearchMode;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;

/// The tool to advertise for `config`, or `None` when the deployment offers no
/// search — which is not the same as an empty tool, since a `web_search` entry
/// present at all is one the model may call.
#[must_use]
pub(crate) fn tool(config: &WebSearchConfig) -> Option<Value> {
    let (external, indexed) = match config.mode {
        WebSearchMode::Disabled => return None,
        WebSearchMode::Cached => (false, None),
        WebSearchMode::Indexed => (true, Some(true)),
        WebSearchMode::Live => (true, None),
    };

    let mut spec = Map::new();
    spec.insert("type".to_string(), json!("web_search"));
    spec.insert("external_web_access".to_string(), json!(external));
    if let Some(indexed) = indexed {
        spec.insert("indexed_web_access".to_string(), json!(indexed));
    }
    spec.insert(
        "search_context_size".to_string(),
        json!(config.context_size.as_str()),
    );
    if !config.allowed_domains.is_empty() {
        spec.insert(
            "filters".to_string(),
            json!({ "allowed_domains": config.allowed_domains }),
        );
    }
    if let Some(location) = config.user_location.as_ref().filter(|l| !l.is_empty()) {
        let mut wire = Map::new();
        // The only kind this API takes, and stating it is not optional: a
        // location object without it is rejected.
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
    if config.include_images {
        spec.insert("search_content_types".to_string(), json!(["text", "image"]));
    }
    Some(Value::Object(spec))
}
