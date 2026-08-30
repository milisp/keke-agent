//! What xAI says it serves, and how to read it.
//!
//! The pay-per-token API answers `/models` with the plain OpenAI listing — ids
//! and little else. The subscription proxy answers the same path with a richer
//! one: a display name, a context window, and the reasoning levels each model
//! takes, spelled `reasoning_efforts` with a `value` per entry. Both are
//! decoded here, because which arrives depends on the credential and neither
//! should be flattened to the other.

use keke_provider_api::ModelInfo;
use keke_provider_api::ReasoningEffort;
use serde::Deserialize;

/// xAI's own default models, compiled in.
///
/// A floor for a picker drawn before the first successful fetch — offline, on
/// a fresh install, behind a proxy. Any fetch that succeeds replaces it.
static BUNDLED: std::sync::LazyLock<Vec<ModelInfo>> =
    std::sync::LazyLock::new(|| keke_catalog::bundled(include_str!("ported/grok/models.json")));

/// xAI's compiled-in catalog.
#[must_use]
pub(crate) fn bundled() -> Vec<ModelInfo> {
    BUNDLED.clone()
}

#[derive(Debug, Deserialize)]
struct Listing {
    #[serde(default)]
    data: Vec<Entry>,
}

#[derive(Debug, Deserialize)]
struct Entry {
    id: String,
    /// xAI's own display name. `name` on the subscription listing; absent on
    /// the plain one.
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    context_window: Option<u64>,
    #[serde(default)]
    max_completion_tokens: Option<u64>,
    #[serde(default)]
    input_modalities: Option<Vec<String>>,
    #[serde(default)]
    reasoning_efforts: Vec<Level>,
    /// What the vendor starts at. Also carried per level as `default: true`,
    /// which wins when both are present because it is the one attached to a
    /// level that is actually on offer.
    #[serde(default)]
    reasoning_effort: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Level {
    value: String,
    #[serde(default)]
    default: bool,
}

/// Decode an xAI listing.
///
/// A level keke has no name for costs that level and not the catalog: xAI adds
/// rungs, and refusing the whole listing the day one appeared would leave a
/// person with no models rather than the ones keke understands.
pub(crate) fn parse(body: &str) -> Result<Vec<ModelInfo>, serde_json::Error> {
    let listing: Listing = serde_json::from_str(body)?;
    Ok(listing.data.into_iter().map(ModelInfo::from).collect())
}

impl From<Entry> for ModelInfo {
    fn from(entry: Entry) -> Self {
        let mut efforts: Vec<ReasoningEffort> = entry
            .reasoning_efforts
            .iter()
            .filter_map(|level| ReasoningEffort::parse(&level.value).ok())
            .collect();
        // xAI publishes strongest first; every surface reads the ladder the
        // other way, so it is ordered once here rather than at each of them.
        efforts.sort_unstable();
        efforts.dedup();

        let flagged = entry
            .reasoning_efforts
            .iter()
            .find(|level| level.default)
            .and_then(|level| ReasoningEffort::parse(&level.value).ok());

        let mut model = ModelInfo::new(entry.id);
        if let Some(name) = entry.name {
            model.display_name = name;
        }
        model.description = entry.description;
        model.context_window = entry.context_window;
        model.max_output_tokens = entry.max_completion_tokens;
        model.supports_vision = entry
            .input_modalities
            .as_ref()
            .is_some_and(|kinds| kinds.iter().any(|kind| kind == "image"));
        model.default_reasoning_effort = flagged
            .or_else(|| {
                entry
                    .reasoning_effort
                    .as_deref()
                    .and_then(|level| ReasoningEffort::parse(level).ok())
            })
            // A default that names a level this model does not offer is not a
            // default; picking one of the others would buy different thinking
            // than the vendor asked for.
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
        let latest = models.first().expect("at least one model");
        assert_eq!(latest.id, "grok-4.6");
        assert_eq!(latest.display_name, "Grok 4.6");
        assert_eq!(latest.starting_effort(), Some(ReasoningEffort::High));
        assert_eq!(
            latest.reasoning_efforts,
            vec![
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
                ReasoningEffort::XHigh,
            ]
        );
    }

    /// The ladder is ordered so that every surface can read it the same way,
    /// whichever order the vendor happened to send.
    #[test]
    fn the_subscription_listing_is_reordered_weakest_first() {
        let models = parse(
            r#"{"data":[{"id":"grok-4.6","name":"Grok 4.6","context_window":500000,
                "reasoning_efforts":[{"value":"xhigh"},{"value":"high","default":true},
                                     {"value":"medium"},{"value":"low"}]}]}"#,
        )
        .expect("decodes");
        let model = models.first().expect("one model");
        assert_eq!(model.display_name, "Grok 4.6");
        assert_eq!(model.context_window, Some(500_000));
        assert_eq!(
            model.reasoning_efforts,
            vec![
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
                ReasoningEffort::XHigh,
            ]
        );
        assert_eq!(model.starting_effort(), Some(ReasoningEffort::High));
    }

    #[test]
    fn a_level_keke_has_no_name_for_drops_the_level_and_not_the_model() {
        let models = parse(
            r#"{"data":[{"id":"grok-next","reasoning_effort":"stellar",
                "reasoning_efforts":[{"value":"high"},{"value":"stellar"}]}]}"#,
        )
        .expect("decodes");
        let model = models.first().expect("one model");
        assert_eq!(model.reasoning_efforts, vec![ReasoningEffort::High]);
        assert_eq!(model.starting_effort(), None);
    }

    #[test]
    fn the_plain_listing_is_still_a_list_of_ids() {
        let models = parse(
            r#"{"data":[{"id":"grok-4.6","input_modalities":["text","image"]},
                                       {"id":"grok-3-mini"}]}"#,
        )
        .expect("decodes");
        assert_eq!(models.len(), 2);
        assert!(models[0].supports_vision);
        assert!(!models[0].supports_reasoning());
        assert_eq!(models[1].display_name, "grok-3-mini");
    }
}
