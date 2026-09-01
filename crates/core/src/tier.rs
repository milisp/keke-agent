use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;

use keke_protocol::ServiceTier;

/// The queue a running session asks to be answered from, changeable while it
/// runs.
///
/// Beside the config for the same reason [`crate::EffortSwitch`] is: a person
/// turning fast mode on is talking about the next answer, not about the next
/// session, and the turn loop must see the change through a shared handle.
///
/// `None` stays a state of its own — no tier named, the endpoint routes as it
/// would anyway — because naming the standard queue would override a default
/// the account may have set elsewhere, and this must not collapse the two.
#[derive(Debug)]
pub struct ServiceTierSwitch(AtomicU8);

impl ServiceTierSwitch {
    #[must_use]
    pub fn new(tier: Option<ServiceTier>) -> Self {
        Self(AtomicU8::new(encode(tier)))
    }

    #[must_use]
    pub fn get(&self) -> Option<ServiceTier> {
        decode(self.0.load(Ordering::Relaxed))
    }

    pub fn set(&self, tier: Option<ServiceTier>) {
        self.0.store(encode(tier), Ordering::Relaxed);
    }
}

fn encode(tier: Option<ServiceTier>) -> u8 {
    match tier {
        None => 0,
        Some(ServiceTier::Fast) => 1,
        Some(ServiceTier::Flex) => 2,
    }
}

fn decode(value: u8) -> Option<ServiceTier> {
    match value {
        1 => Some(ServiceTier::Fast),
        2 => Some(ServiceTier::Flex),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_switch_shares_the_tier_it_is_given() {
        let switch = std::sync::Arc::new(ServiceTierSwitch::new(None));
        let held = std::sync::Arc::clone(&switch);
        switch.set(Some(ServiceTier::Fast));
        assert_eq!(held.get(), Some(ServiceTier::Fast));
    }

    /// Naming no tier is not naming the standard one: round-tripping must keep
    /// them apart.
    #[test]
    fn unset_is_not_a_tier() {
        let switch = ServiceTierSwitch::new(Some(ServiceTier::Flex));
        assert_eq!(switch.get(), Some(ServiceTier::Flex));
        switch.set(None);
        assert_eq!(switch.get(), None);
    }
}
