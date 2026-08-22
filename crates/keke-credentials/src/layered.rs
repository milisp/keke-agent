use keke_auth_api::CredentialOrigin;
use keke_auth_api::CredentialRef;
use keke_auth_api::CredentialStore;
use keke_auth_api::StoreError;

use crate::EnvStore;
use crate::FileStore;
use crate::KeyringStore;

struct Layer {
    store: Box<dyn CredentialStore>,
    origin: CredentialOrigin,
}

/// Several stores consulted in order: the first that holds a reference wins.
///
/// The order is also the write policy. Writes go to the first writable layer,
/// and a reference already held by a read-only layer *above* that target is
/// refused rather than written, because the write would report success while
/// every subsequent read kept returning the shadowing value — the failure mode
/// [`StoreError::Shadowed`] exists to name.
#[derive(Default)]
pub struct LayeredStore {
    layers: Vec<Layer>,
}

impl LayeredStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a layer below the ones already added.
    ///
    /// `origin` is passed rather than taken from the store because writability
    /// has to be known before a reference resolves, and `describe` only speaks
    /// about references that are present.
    #[must_use]
    pub fn layer(
        mut self,
        store: impl CredentialStore + 'static,
        origin: CredentialOrigin,
    ) -> Self {
        self.layers.push(Layer {
            store: Box::new(store),
            origin,
        });
        self
    }

    fn write_target(&self) -> Option<usize> {
        self.layers.iter().position(|layer| layer.origin.writable)
    }
}

/// The default composition: keyring over `<home>/credentials.json` over the
/// environment.
///
/// A machine with no usable keyring drops that layer with a warning instead of
/// failing every read, so an SSH session or a container degrades to the file
/// layer rather than to no credentials at all.
pub fn standard_store(service: &str, file: FileStore) -> LayeredStore {
    let keyring = KeyringStore::new(service);
    let mut store = LayeredStore::new();
    if keyring.available() {
        store = store.layer(keyring, KeyringStore::origin());
    }
    store
        .layer(file, FileStore::origin())
        .layer(EnvStore, EnvStore::origin())
}

impl CredentialStore for LayeredStore {
    fn load(&self, name: &CredentialRef) -> Result<Option<String>, StoreError> {
        for layer in &self.layers {
            if let Some(value) = layer.store.load(name)? {
                return Ok(Some(value));
            }
        }
        Ok(None)
    }

    fn describe(&self, name: &CredentialRef) -> Result<Option<CredentialOrigin>, StoreError> {
        for layer in &self.layers {
            if layer.store.describe(name)?.is_some() {
                return Ok(Some(layer.origin.clone()));
            }
        }
        Ok(None)
    }

    fn save(&self, name: &CredentialRef, value: &str) -> Result<(), StoreError> {
        let target = self.write_target().ok_or_else(|| {
            StoreError::Backend("no writable credential layer is configured".to_string())
        })?;
        for layer in &self.layers[..target] {
            if layer.store.describe(name)?.is_some() {
                return Err(StoreError::Shadowed {
                    name: name.to_string(),
                    origin: layer.origin.source.clone(),
                });
            }
        }
        self.layers[target].store.save(name, value)
    }

    /// Removes `name` from every writable layer, so a delete is not undone by a
    /// stale copy one layer down.
    fn delete(&self, name: &CredentialRef) -> Result<bool, StoreError> {
        let mut removed = false;
        for layer in self.layers.iter().filter(|layer| layer.origin.writable) {
            removed |= layer.store.delete(name)?;
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryStore;

    /// Stands in for [`EnvStore`], which cannot be populated from a test
    /// without mutating the process environment.
    struct ReadOnlyEnv(MemoryStore);

    impl ReadOnlyEnv {
        fn holding(name: &CredentialRef, value: &str) -> Self {
            let inner = MemoryStore::new();
            inner.save(name, value).unwrap();
            Self(inner)
        }
    }

    impl CredentialStore for ReadOnlyEnv {
        fn load(&self, name: &CredentialRef) -> Result<Option<String>, StoreError> {
            self.0.load(name)
        }

        fn describe(&self, name: &CredentialRef) -> Result<Option<CredentialOrigin>, StoreError> {
            Ok(self.0.load(name)?.map(|_| EnvStore::origin()))
        }

        fn save(&self, name: &CredentialRef, _value: &str) -> Result<(), StoreError> {
            Err(StoreError::Shadowed {
                name: name.to_string(),
                origin: EnvStore::SOURCE.to_string(),
            })
        }

        fn delete(&self, name: &CredentialRef) -> Result<bool, StoreError> {
            let _ = name;
            Ok(false)
        }
    }

    fn key() -> CredentialRef {
        CredentialRef::new("XAI_API_KEY").unwrap()
    }

    #[test]
    fn an_empty_stored_value_reads_back_as_absent() {
        let name = key();
        let store = LayeredStore::new().layer(MemoryStore::new(), MemoryStore::origin());
        store.save(&name, "").unwrap();
        assert_eq!(store.load(&name).unwrap(), None);
        assert_eq!(store.describe(&name).unwrap(), None);
    }

    #[test]
    fn an_empty_upper_layer_does_not_hide_a_lower_one() {
        let name = key();
        let blank = MemoryStore::new();
        blank.save(&name, "  ").unwrap();
        let filled = MemoryStore::new();
        filled.save(&name, "real").unwrap();
        let store = LayeredStore::new()
            .layer(blank, MemoryStore::origin())
            .layer(filled, MemoryStore::origin());
        assert_eq!(store.load(&name).unwrap().as_deref(), Some("real"));
    }

    #[test]
    fn a_write_shadowed_by_a_read_only_layer_is_rejected() {
        let name = key();
        let store = LayeredStore::new()
            .layer(ReadOnlyEnv::holding(&name, "from-env"), EnvStore::origin())
            .layer(MemoryStore::new(), MemoryStore::origin());

        let err = store.save(&name, "from-login").unwrap_err();
        assert!(
            matches!(&err, StoreError::Shadowed { name: n, origin } if n == "XAI_API_KEY" && origin == "env"),
            "got {err:?}"
        );
        assert_eq!(store.load(&name).unwrap().as_deref(), Some("from-env"));
    }

    #[test]
    fn a_read_only_layer_below_the_write_target_does_not_block_a_write() {
        let name = key();
        let store = LayeredStore::new()
            .layer(MemoryStore::new(), MemoryStore::origin())
            .layer(ReadOnlyEnv::holding(&name, "from-env"), EnvStore::origin());

        store.save(&name, "from-login").unwrap();
        assert_eq!(store.load(&name).unwrap().as_deref(), Some("from-login"));
    }

    #[test]
    fn describe_names_the_layer_that_would_answer() {
        let name = key();
        let store = LayeredStore::new()
            .layer(MemoryStore::new(), MemoryStore::origin())
            .layer(ReadOnlyEnv::holding(&name, "from-env"), EnvStore::origin());

        let origin = store.describe(&name).unwrap().unwrap();
        assert_eq!(origin.source, "env");
        assert!(!origin.writable);
    }

    #[test]
    fn delete_clears_every_writable_layer() {
        let name = key();
        let upper = MemoryStore::new();
        upper.save(&name, "a").unwrap();
        let lower = MemoryStore::new();
        lower.save(&name, "b").unwrap();
        let store = LayeredStore::new()
            .layer(upper, MemoryStore::origin())
            .layer(lower, MemoryStore::origin());

        assert!(store.delete(&name).unwrap());
        assert_eq!(store.load(&name).unwrap(), None);
        assert!(!store.delete(&name).unwrap());
    }
}
