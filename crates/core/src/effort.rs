//! The live reasoning_effort setting.

use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;

use keke_protocol::ReasoningEffort;

/// The effort level a running session asks for, changeable while it runs.
///
/// Beside the config rather than in it for the same reason
/// [`crate::ApprovalSwitch`] is: a person raising the level is talking about
/// the next answer, not about the next session, and the turn loop must see the
/// change through a shared handle rather than only whoever set it.
///
/// `None` stays a state of its own — "unset, let the model decide" — because
/// the config distinguishes it from the lowest rung and this must not collapse
/// the two.
#[derive(Debug)]
pub struct EffortSwitch(AtomicU8);

impl EffortSwitch {
    #[must_use]
    pub fn new(effort: Option<ReasoningEffort>) -> Self {
        Self(AtomicU8::new(encode(effort)))
    }

    #[must_use]
    pub fn get(&self) -> Option<ReasoningEffort> {
        decode(self.0.load(Ordering::Relaxed))
    }

    pub fn set(&self, effort: Option<ReasoningEffort>) {
        self.0.store(encode(effort), Ordering::Relaxed);
    }
}

fn encode(effort: Option<ReasoningEffort>) -> u8 {
    match effort {
        None => 0,
        Some(ReasoningEffort::Low) => 1,
        Some(ReasoningEffort::Medium) => 2,
        Some(ReasoningEffort::High) => 3,
        Some(ReasoningEffort::XHigh) => 4,
        Some(ReasoningEffort::Max) => 5,
        Some(ReasoningEffort::Ultra) => 6,
    }
}

fn decode(value: u8) -> Option<ReasoningEffort> {
    match value {
        1 => Some(ReasoningEffort::Low),
        2 => Some(ReasoningEffort::Medium),
        3 => Some(ReasoningEffort::High),
        4 => Some(ReasoningEffort::XHigh),
        5 => Some(ReasoningEffort::Max),
        6 => Some(ReasoningEffort::Ultra),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_switch_shares_the_level_it_is_given() {
        let switch = std::sync::Arc::new(EffortSwitch::new(None));
        let held = std::sync::Arc::clone(&switch);
        switch.set(Some(ReasoningEffort::Max));
        assert_eq!(held.get(), Some(ReasoningEffort::Max));
    }

    /// Unset is not the bottom rung: round-tripping must keep them apart.
    #[test]
    fn unset_is_not_the_lowest_level() {
        let switch = EffortSwitch::new(Some(ReasoningEffort::Low));
        assert_eq!(switch.get(), Some(ReasoningEffort::Low));
        switch.set(None);
        assert_eq!(switch.get(), None);
    }
}
