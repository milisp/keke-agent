use serde::Deserialize;
use serde::Serialize;

/// Which queue a vendor should answer from, when it sells more than one speed.
///
/// Neutral for the same reason [`ReasoningEffort`](crate::ReasoningEffort) is,
/// and orthogonal to it: the tier does not change how hard the model thinks or
/// what it is shown, only how quickly and at what price the same answer comes
/// back. Every vendor that offers it spells it differently — OpenAI calls the
/// fast queue `priority` — so the choice is made once in keke's terms and each
/// provider translates it on its own wire.
///
/// Absence is not a third tier. `None` means no tier was named, leaving the
/// endpoint to route the way it would anyway; naming the standard queue instead
/// would override a default the account may have set elsewhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceTier {
    /// The same answer, sooner, against a larger share of the account's
    /// allowance. This is what a surface calls "fast mode".
    #[serde(alias = "priority")]
    Fast,
    /// The deferred queue: cheaper, and slower under load.
    Flex,
}

impl ServiceTier {
    /// The spelling surfaces show and configuration is written in — the word a
    /// person uses, not the queue's name at any one vendor.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Flex => "flex",
        }
    }

    /// The inverse of [`Self::as_str`], accepting a vendor's own spelling too
    /// so a config copied from one is not rejected for being right. `None` for
    /// anything else, including a tier from a future build — a routing this
    /// build cannot ask for must never be read as one it can.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "fast" | "priority" => Some(Self::Fast),
            "flex" => Some(Self::Flex),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The queue's name at a vendor is not the word a person writes, and a
    /// config that used either must mean the same thing.
    #[test]
    fn fast_and_priority_name_the_same_queue() {
        assert_eq!(ServiceTier::parse("fast"), Some(ServiceTier::Fast));
        assert_eq!(ServiceTier::parse("priority"), Some(ServiceTier::Fast));
        assert_eq!(ServiceTier::Fast.as_str(), "fast");
        assert_eq!(ServiceTier::parse("turbo"), None);
    }
}
