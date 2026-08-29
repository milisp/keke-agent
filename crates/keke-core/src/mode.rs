//! The live session mode.

use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;

use keke_config_types::SessionMode;

/// Which mode a running session is in, changeable while it runs.
///
/// Beside the config rather than in it for the same reason
/// [`crate::ApprovalSwitch`] is: a person turning plan mode on is talking about
/// the work in front of them, and everything that has to honour the change —
/// the turn loop, a guard, an extension holding its own lifecycle state — must
/// see it through a shared handle rather than only whoever set it.
///
/// The engine carries the switch and nothing more. What plan mode *means* —
/// which tools it blocks, what the model is told, where the plan is written —
/// is an extension's, because a deployment that wants a different planning
/// discipline should not have to change the engine to get one.
///
/// This is the authority for the coarse value. An extension keeping a finer
/// lifecycle of its own reconciles *to* this at a turn boundary rather than the
/// other way round, so there is one answer to "is this session planning?" and
/// it is the one a person last chose.
#[derive(Debug)]
pub struct SessionModeSwitch(AtomicU8);

impl SessionModeSwitch {
    #[must_use]
    pub fn new(mode: SessionMode) -> Self {
        Self(AtomicU8::new(encode(mode)))
    }

    #[must_use]
    pub fn get(&self) -> SessionMode {
        decode(self.0.load(Ordering::Relaxed))
    }

    pub fn set(&self, mode: SessionMode) {
        self.0.store(encode(mode), Ordering::Relaxed);
    }
}

impl Default for SessionModeSwitch {
    fn default() -> Self {
        Self::new(SessionMode::default())
    }
}

fn encode(mode: SessionMode) -> u8 {
    match mode {
        SessionMode::Default => 0,
        SessionMode::Plan => 1,
    }
}

fn decode(value: u8) -> SessionMode {
    match value {
        1 => SessionMode::Plan,
        _ => SessionMode::Default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_switch_shares_the_mode_it_is_given() {
        let switch = std::sync::Arc::new(SessionModeSwitch::new(SessionMode::Default));
        let held = std::sync::Arc::clone(&switch);
        switch.set(SessionMode::Plan);
        assert_eq!(held.get(), SessionMode::Plan);
    }

    #[test]
    fn a_fresh_switch_is_not_planning() {
        assert_eq!(SessionModeSwitch::default().get(), SessionMode::Default);
    }
}
