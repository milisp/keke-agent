//! What contributors are handed, and what they may hand back.

use std::any::Any;
use std::any::TypeId;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;

use keke_protocol::SessionEvent;
use keke_protocol::SessionId;
use keke_protocol::ThreadId;
use keke_protocol::TurnId;

/// A piece of model-visible context contributed by an extension.
///
/// Ordering is explicit rather than registration-derived so a fragment's
/// position in the prompt is a property of the fragment, not an accident of
/// which extension happened to install first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextFragment {
    /// Stable name, used for logging and de-duplication.
    pub name: String,
    /// Ascending sort key. Convention: negative for harness identity, 0 for the
    /// deployment persona, 100+ for tool guidance.
    pub order: i32,
    pub text: String,
}

impl ContextFragment {
    #[must_use]
    pub fn new(name: impl Into<String>, order: i32, text: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            order,
            text: text.into(),
        }
    }
}

/// Scoped state contributors share without touching engine internals.
///
/// A type map rather than a fixed struct, so adding an extension never means
/// editing a shared context type.
#[derive(Default)]
struct TypeMap(RwLock<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>);

impl TypeMap {
    fn insert<T: Any + Send + Sync>(&self, value: T) {
        if let Ok(mut map) = self.0.write() {
            map.insert(TypeId::of::<T>(), Arc::new(value));
        }
    }

    fn get<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
        let map = self.0.read().ok()?;
        map.get(&TypeId::of::<T>()).cloned()?.downcast::<T>().ok()
    }
}

/// The handle every contributor receives.
#[derive(Clone)]
pub struct ExtensionContext {
    pub session: SessionId,
    pub thread: ThreadId,
    turn: Option<TurnId>,
    events: Arc<RwLock<Vec<SessionEvent>>>,
    state: Arc<TypeMap>,
}

impl ExtensionContext {
    #[must_use]
    pub fn new(session: SessionId, thread: ThreadId) -> Self {
        Self {
            session,
            thread,
            turn: None,
            events: Arc::new(RwLock::new(Vec::new())),
            state: Arc::new(TypeMap::default()),
        }
    }

    /// Name the turn this context belongs to.
    ///
    /// Set by the engine, which is the only thing that knows the id. A
    /// contributor reads it to stamp the events it records.
    #[must_use]
    pub fn in_turn(mut self, turn: TurnId) -> Self {
        self.turn = Some(turn);
        self
    }

    /// The turn in progress, when this context belongs to one.
    #[must_use]
    pub fn turn(&self) -> Option<TurnId> {
        self.turn
    }

    /// Queue a session event for the engine to append to the rollout log.
    ///
    /// An extension that puts something in front of the model has to be able to
    /// record it, or the log stops being a full account of the turn
    /// (`AGENTS.md` invariant 6). Queued rather than written because appending
    /// is asynchronous and owns the recorder; the engine drains this between
    /// steps, so ordering within a step is registration order and not
    /// completion order.
    pub fn record(&self, event: SessionEvent) {
        if let Ok(mut events) = self.events.write() {
            events.push(event);
        }
    }

    /// Take everything recorded since the last drain. Called by the engine.
    #[must_use]
    pub fn drain_events(&self) -> Vec<SessionEvent> {
        self.events
            .write()
            .map(|mut events| std::mem::take(&mut *events))
            .unwrap_or_default()
    }

    /// Store a value keyed by its type, replacing any previous value.
    pub fn insert<T: Any + Send + Sync>(&self, value: T) {
        self.state.insert(value);
    }

    /// Retrieve a previously stored value.
    #[must_use]
    pub fn get<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
        self.state.get::<T>()
    }
}

impl std::fmt::Debug for ExtensionContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtensionContext")
            .field("session", &self.session)
            .field("thread", &self.thread)
            .field("turn", &self.turn)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq)]
    struct Counter(u32);

    #[test]
    fn type_map_round_trips() {
        let ctx = ExtensionContext::new(SessionId::new(), ThreadId::new());
        assert!(ctx.get::<Counter>().is_none());
        ctx.insert(Counter(7));
        assert_eq!(ctx.get::<Counter>().as_deref(), Some(&Counter(7)));
    }
}
