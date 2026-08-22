//! What contributors are handed, and what they may hand back.

use std::any::Any;
use std::any::TypeId;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;

use keke_protocol::SessionId;
use keke_protocol::ThreadId;

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
    state: Arc<TypeMap>,
}

impl ExtensionContext {
    #[must_use]
    pub fn new(session: SessionId, thread: ThreadId) -> Self {
        Self {
            session,
            thread,
            state: Arc::new(TypeMap::default()),
        }
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
