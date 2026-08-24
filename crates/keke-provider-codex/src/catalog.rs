//! What OpenAI says it serves, and how to read it.
//!
//! There are two listings behind one route. The public API answers
//! `{"data":[{"id":…}]}` — a bag of ids and nothing else. The ChatGPT backend
//! answers `{"models":[…]}`, which is the interesting one: display names,
//! descriptions, context windows, and the reasoning levels each model accepts.
//! Which arrives depends on the credential, so both are decoded here and the
//! richer one is not thrown away when it turns up.

use keke_provider_api::ModelInfo;
use keke_provider_api::ReasoningEffort;
use serde::Deserialize;

/// The listed models OpenAI publishes, compiled in.
///
/// A floor rather than the answer: it is what a picker shows before the first
/// successful fetch, on a plane, or behind a proxy. Any fetch that succeeds
/// replaces it wholesale.
static BUNDLED: std::sync::LazyLock<Vec<ModelInfo>> =
    std::sync::LazyLock::new(|| keke_catalog::bundled(include_str!("ported/codex/models.json")));

/// OpenAI's compiled-in catalog.
#[must_use]
pub(crate) fn bundled() -> Vec<ModelInfo> {
    BUNDLED.clone()
}

/// Either listing, whichever the endpoint sent.
#[derive(Debug, Deserialize)]
struct Listing {
    /// The ChatGPT backend's key.
    #[serde(default)]
    models: Vec<Rich>,
    /// The public API's key.
    #[serde(default)]
    data: Vec<Plain>,
}

#[derive(Debug, Deserialize)]
struct Plain {
    id: String,
}

#[derive(Debug, Deserialize)]
struct Rich {
    slug: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    context_window: Option<u64>,
    #[serde(default)]
    input_modalities: Option<Vec<String>>,
    #[serde(default)]
    supported_reasoning_levels: Vec<Level>,
    #[serde(default)]
    default_reasoning_level: Option<String>,
    /// `hide` marks a model the vendor does not want in a picker — an internal
    /// alias, a review-only model. Absent means listed, because a listing that
    /// forgot to say is one keke should still show.
    #[serde(default)]
    visibility: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Level {
    effort: String,
}

/// Decode whichever listing came back.
///
/// A level keke has no name for is dropped rather than failing the listing:
/// OpenAI adds rungs, and a keke that refused the whole catalog the day one
/// appeared would show no models at all rather than the five it understands.
pub(crate) fn parse(body: &str) -> Result<Vec<ModelInfo>, serde_json::Error> {
    let listing: Listing = serde_json::from_str(body)?;
    if !listing.models.is_empty() {
        return Ok(listing
            .models
            .into_iter()
            .filter(|model| model.visibility.as_deref() != Some("hide"))
            .map(ModelInfo::from)
            .collect());
    }
    Ok(listing
        .data
        .into_iter()
        .map(|plain| plain.id)
        .map(ModelInfo::new)
        .collect())
}

impl From<Rich> for ModelInfo {
    fn from(rich: Rich) -> Self {
        let efforts: Vec<ReasoningEffort> = rich
            .supported_reasoning_levels
            .iter()
            .filter_map(|level| ReasoningEffort::parse(&level.effort).ok())
            .collect();
        let mut model = ModelInfo::new(rich.slug);
        if let Some(name) = rich.display_name {
            model.display_name = name;
        }
        model.description = rich.description;
        model.context_window = rich.context_window;
        model.supports_vision = rich
            .input_modalities
            .as_ref()
            .is_some_and(|kinds| kinds.iter().any(|kind| kind == "image"));
        model.default_reasoning_effort = rich
            .default_reasoning_level
            .as_deref()
            .and_then(|level| ReasoningEffort::parse(level).ok())
            // A default keke dropped as unknown must not become one of the
            // levels it did understand: that would quietly buy different
            // thinking than the vendor intended.
            .filter(|effort| efforts.contains(effort));
        model.reasoning_efforts = efforts;
        model
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bundled_catalog_decodes_and_carries_its_levels() {
        let models = bundled();
        let top = models.first().expect("at least one model");
        assert!(top.supports_reasoning());
        assert_eq!(top.starting_effort(), Some(ReasoningEffort::Low));
        assert!(models.iter().any(|model| model.id == "gpt-5.5"));
    }

    /// The whole point of the rich listing: a picker gets a name and a ladder,
    /// not just a slug.
    #[test]
    fn the_subscription_listing_carries_names_and_levels() {
        let models = parse(
            r#"{"models":[{"slug":"gpt-5.6-sol","display_name":"GPT-5.6-Sol",
                "context_window":272000,"input_modalities":["text","image"],
                "default_reasoning_level":"low","visibility":"list",
                "supported_reasoning_levels":[{"effort":"low"},{"effort":"max"}]}]}"#,
        )
        .expect("decodes");
        let model = models.first().expect("one model");
        assert_eq!(model.display_name, "GPT-5.6-Sol");
        assert_eq!(model.context_window, Some(272_000));
        assert!(model.supports_vision);
        assert_eq!(
            model.reasoning_efforts,
            vec![ReasoningEffort::Low, ReasoningEffort::Max]
        );
        assert_eq!(model.starting_effort(), Some(ReasoningEffort::Low));
    }

    #[test]
    fn a_hidden_model_is_not_offered() {
        let models = parse(
            r#"{"models":[{"slug":"visible","visibility":"list"},
                          {"slug":"internal","visibility":"hide"}]}"#,
        )
        .expect("decodes");
        let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(ids, vec!["visible"]);
    }

    /// A rung added by the vendor must cost that one level, not the catalog.
    #[test]
    fn a_level_keke_has_no_name_for_drops_the_level_and_not_the_model() {
        let models = parse(
            r#"{"models":[{"slug":"gpt-next","default_reasoning_level":"cosmic",
                "supported_reasoning_levels":[{"effort":"high"},{"effort":"cosmic"}]}]}"#,
        )
        .expect("decodes");
        let model = models.first().expect("one model");
        assert_eq!(model.reasoning_efforts, vec![ReasoningEffort::High]);
        // And the default it could not read does not become `high`.
        assert_eq!(model.starting_effort(), None);
    }

    #[test]
    fn the_public_api_listing_is_still_a_list_of_ids() {
        let models = parse(r#"{"data":[{"id":"gpt-5.5"},{"id":"o3"}]}"#).expect("decodes");
        let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(ids, vec!["gpt-5.5", "o3"]);
        assert!(!models[0].supports_reasoning());
    }
}
