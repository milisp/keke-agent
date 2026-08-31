//! xAI's hosted search, translated from the neutral [`WebSearchConfig`].
//!
//! xAI puts the same capability in two different places depending on the
//! address. Its chat-completions API takes a top-level `search_parameters`
//! object beside `messages`; its responses API takes a `web_search` entry in
//! the tool list, the way OpenAI's does. Both are built here so the choice
//! follows the endpoint's wire rather than being a second thing to configure.
//!
//! [`WebSearchConfig::include_images`] has no counterpart at this vendor and is
//! dropped rather than refused: it asks for *more* than xAI's search returns, so
//! ignoring it under-delivers where ignoring a restriction would over-grant.

use keke_config_types::WebSearchConfig;
use keke_config_types::WebSearchContextSize;
use keke_config_types::WebSearchMode;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;

/// How many results each context size buys.
///
/// xAI counts results where OpenAI sizes a context budget, so the ladder is
/// mapped rather than passed through. These are not deployment-varying
/// constants in the sense invariant 9 forbids: the knob a deployment turns is
/// `web_search.context_size`, and this is only what this vendor calls it.
const fn max_results(size: WebSearchContextSize) -> u32 {
    match size {
        WebSearchContextSize::Low => 5,
        WebSearchContextSize::Medium => 15,
        WebSearchContextSize::High => 30,
    }
}

/// What xAI is asked to do, or `None` when the deployment offers no search.
///
/// `auto` rather than `on`: an offered search the model may decline is the same
/// bargain as an advertised tool, whereas `on` searches on every turn including
/// the ones that need no web at all, and bills for it.
///
/// [`WebSearchMode::Cached`] and [`WebSearchMode::Indexed`] are refused instead
/// of approximated. xAI's search is a live fetch with no cached-only or
/// index-confined tier, so honouring either as best-effort would hand live
/// outbound access to precisely the deployment that wrote down it may not have
/// it — invariant 8's case exactly.
fn wire_mode(mode: WebSearchMode) -> Result<Option<&'static str>, String> {
    match mode {
        WebSearchMode::Disabled => Ok(None),
        WebSearchMode::Live => Ok(Some("auto")),
        WebSearchMode::Cached | WebSearchMode::Indexed => Err(format!(
            "xAI's hosted search always fetches live, so web_search.mode `{}` cannot be honored \
             here; use `live` to accept live fetches or `disabled` to offer no search",
            mode.as_str()
        )),
    }
}

/// The `search_parameters` object for the chat-completions wire.
pub(crate) fn parameters(config: &WebSearchConfig) -> Result<Option<Value>, String> {
    let Some(mode) = wire_mode(config.mode)? else {
        return Ok(None);
    };

    let mut params = Map::new();
    params.insert("mode".to_string(), json!(mode));
    // Without these the model reports what it read with nothing a person can
    // check it against, which for a search whose fetches the harness never sees
    // is the whole of the audit trail.
    params.insert("return_citations".to_string(), json!(true));
    params.insert(
        "max_search_results".to_string(),
        json!(max_results(config.context_size)),
    );

    // Only stated when something narrows it. Naming sources at all drops the
    // ones left out — xAI defaults to web *and* X — so an unconstrained
    // deployment says nothing and keeps the vendor's own default.
    let country = config
        .user_location
        .as_ref()
        .and_then(|location| location.country.clone());
    if !config.allowed_domains.is_empty() || country.is_some() {
        let mut web = Map::new();
        web.insert("type".to_string(), json!("web"));
        if !config.allowed_domains.is_empty() {
            web.insert(
                "allowed_websites".to_string(),
                json!(config.allowed_domains),
            );
        }
        if let Some(country) = &country {
            web.insert("country".to_string(), json!(country));
        }
        params.insert("sources".to_string(), json!([Value::Object(web)]));
    }

    Ok(Some(Value::Object(params)))
}

/// The `web_search` tool entry for the responses wire.
pub(crate) fn tool(config: &WebSearchConfig) -> Result<Option<Value>, String> {
    if wire_mode(config.mode)?.is_none() {
        return Ok(None);
    }

    let mut spec = Map::new();
    spec.insert("type".to_string(), json!("web_search"));
    if !config.allowed_domains.is_empty() {
        spec.insert(
            "filters".to_string(),
            json!({ "allowed_domains": config.allowed_domains }),
        );
    }
    Ok(Some(Value::Object(spec)))
}

/// The domains one search may reach.
///
/// A configured allowlist is authoritative and the model's own list narrows it
/// rather than replacing it. Letting the model's list win would make the
/// deployment's setting advisory — a model that wanted a domain the config
/// excluded need only name it — and the approval seam's rule is the same one:
/// composing two opinions about reach may restrict and may never widen.
pub(crate) fn confine(config: &WebSearchConfig, requested: &[String]) -> Vec<String> {
    if config.allowed_domains.is_empty() {
        return requested.to_vec();
    }
    if requested.is_empty() {
        return config.allowed_domains.clone();
    }
    config
        .allowed_domains
        .iter()
        .filter(|domain| requested.iter().any(|wanted| wanted == *domain))
        .cloned()
        .collect()
}

/// One search as a self-contained request on the responses wire.
pub(crate) fn responses_request(
    model: &str,
    query: &str,
    domains: &[String],
) -> Result<Value, String> {
    let mut spec = Map::new();
    spec.insert("type".to_string(), json!("web_search"));
    if !domains.is_empty() {
        spec.insert("filters".to_string(), json!({ "allowed_domains": domains }));
    }
    Ok(json!({
        "model": model,
        "input": query,
        "tools": [Value::Object(spec)],
        // The one tool in the list, so nothing competes with it for the
        // model's attention — which is the whole reason this call is separate
        // from the conversation's.
        "tool_choice": "auto",
        "store": false,
        "stream": false,
    }))
}

/// One search as a self-contained request on the chat-completions wire.
pub(crate) fn chat_request(
    config: &WebSearchConfig,
    model: &str,
    query: &str,
    domains: &[String],
) -> Result<Value, String> {
    let mut confined = config.clone();
    confined.allowed_domains = domains.to_vec();
    let Some(params) = parameters(&confined)? else {
        return Err("web search is disabled for this endpoint".to_string());
    };
    Ok(json!({
        "model": model,
        "messages": [{ "role": "user", "content": query }],
        "search_parameters": params,
        "stream": false,
    }))
}

/// The summary and sources in a responses-wire reply.
///
/// Annotations are where the citations live; the `web_search_call` items the
/// same reply carries are the vendor narrating its own work and hold no URL.
pub(crate) fn read_responses(body: &Value) -> (String, Vec<(String, Option<String>)>) {
    let mut summary = String::new();
    let mut citations = Vec::new();
    let items = body.get("output").and_then(Value::as_array);
    for item in items.into_iter().flatten() {
        for part in item
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                summary.push_str(text);
            }
            for annotation in part
                .get("annotations")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(url) = annotation.get("url").and_then(Value::as_str) {
                    citations.push((
                        url.to_string(),
                        annotation
                            .get("title")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    ));
                }
            }
        }
    }
    (summary, citations)
}

/// The summary and sources in a chat-completions reply.
///
/// This wire reports its sources as a flat `citations` array of URLs beside
/// the choices rather than as annotations inside the text, which is the whole
/// reason the two wires are read separately instead of through one shape that
/// would fit neither.
pub(crate) fn read_chat(body: &Value) -> (String, Vec<(String, Option<String>)>) {
    let summary = body
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let citations = body
        .get("citations")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|url| (url.to_string(), None))
        .collect();
    (summary, citations)
}
