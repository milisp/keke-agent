use std::sync::Arc;
use std::sync::RwLock;

/// The model a running session asks for, changeable while it runs.
///
/// Beside the config for the same reason [`crate::EffortSwitch`] is: a person
/// changing models is talking about the next answer, not about the next
/// session, and the turn loop must see the change through a shared handle
/// rather than only whoever set it.
///
/// The provider is *not* part of this. Routing to another vendor means other
/// credentials and another client, which is a session rather than a setting.
#[derive(Debug)]
pub struct ModelSwitch(RwLock<Arc<str>>);

impl ModelSwitch {
    #[must_use]
    pub fn new(model: impl Into<Arc<str>>) -> Self {
        Self(RwLock::new(model.into()))
    }

    /// The model the next request will name.
    ///
    /// A poisoned lock answers with the model rather than panicking: a session
    /// that cannot read its own setting has no better answer to give, and
    /// failing the turn over it would lose the conversation.
    #[must_use]
    pub fn get(&self) -> Arc<str> {
        match self.0.read() {
            Ok(model) => Arc::clone(&model),
            Err(poisoned) => Arc::clone(&poisoned.into_inner()),
        }
    }

    pub fn set(&self, model: impl Into<Arc<str>>) {
        let model = model.into();
        match self.0.write() {
            Ok(mut held) => *held = model,
            Err(poisoned) => *poisoned.into_inner() = model,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_switch_shares_the_model_it_is_given() {
        let switch = Arc::new(ModelSwitch::new("grok-4"));
        let held = Arc::clone(&switch);
        switch.set("grok-4.6");
        assert_eq!(&*held.get(), "grok-4.6");
    }
}
