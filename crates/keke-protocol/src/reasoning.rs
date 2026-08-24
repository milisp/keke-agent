use serde::Deserialize;
use serde::Serialize;

/// How hard the model should think before answering.
///
/// One neutral ladder rather than a per-vendor knob: every wire format exposes
/// something like it, but no two agree on the spelling — an effort string here,
/// a token budget there — so the choice is made once in keke's own terms and
/// each wire translates it. A conversation that moves between vendors keeps the
/// setting it was given instead of silently falling back to a vendor default.
///
/// Absence is not another level. `None` means "unset, let the model decide",
/// which is not the same as asking for the least thinking a vendor offers.
///
/// The ladder runs past `High` because vendors have kept adding rungs above it.
/// A level a given endpoint has never heard of is sent as written and rejected
/// by that endpoint, rather than quietly rounded down to one it does know: a
/// setting that silently bought less thinking than it asked for is invisible
/// until the answers are worse.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
    /// One rung above `High`, spelled `xhigh` on the wires that take it.
    #[serde(rename = "xhigh")]
    XHigh,
    /// As much as the model will spend on the answer itself.
    Max,
    /// Above `Max`: the vendors that offer it spend `Max` *and* let the model
    /// break the work up on its own. A rung rather than a separate flag,
    /// because the endpoints that take it take it in the same field.
    Ultra,
}

impl ReasoningEffort {
    /// The spelling the OpenAI wires use, which is also what surfaces show.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
            Self::Ultra => "ultra",
        }
    }

    /// Parse a value typed by a person — a CLI flag or a config file.
    ///
    /// Rejects rather than approximates: a misspelled level that quietly became
    /// the default would be a setting that silently did nothing. `x-high` is
    /// accepted alongside `xhigh` because both spellings are in circulation.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" | "x-high" => Ok(Self::XHigh),
            "max" => Ok(Self::Max),
            "ultra" => Ok(Self::Ultra),
            other => Err(format!(
                "reasoning effort must be one of low, medium, high, xhigh, max, ultra; got `{other}`"
            )),
        }
    }
}

impl std::fmt::Display for ReasoningEffort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ReasoningEffort {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effort_round_trips_through_its_wire_spelling() {
        for effort in [
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::XHigh,
            ReasoningEffort::Max,
            ReasoningEffort::Ultra,
        ] {
            assert_eq!(ReasoningEffort::parse(effort.as_str()), Ok(effort));
            let json = serde_json::to_value(effort).expect("serialize");
            assert_eq!(json, serde_json::json!(effort.as_str()));
        }
    }

    /// The ladder is ordered, so a surface can say "at least high" without
    /// restating which levels exist.
    #[test]
    fn the_ladder_is_ordered_from_least_to_most_thinking() {
        assert!(ReasoningEffort::Low < ReasoningEffort::Medium);
        assert!(ReasoningEffort::High < ReasoningEffort::XHigh);
        assert!(ReasoningEffort::XHigh < ReasoningEffort::Max);
        assert!(ReasoningEffort::Max < ReasoningEffort::Ultra);
    }

    #[test]
    fn both_spellings_of_xhigh_are_accepted() {
        assert_eq!(ReasoningEffort::parse("x-high"), Ok(ReasoningEffort::XHigh));
        assert_eq!(ReasoningEffort::parse("XHigh"), Ok(ReasoningEffort::XHigh));
    }

    #[test]
    fn an_unknown_level_is_refused_rather_than_defaulted() {
        assert!(ReasoningEffort::parse("maximum").is_err());
        assert!(ReasoningEffort::parse("").is_err());
    }
}
