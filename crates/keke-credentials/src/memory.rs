use std::collections::BTreeMap;
use std::sync::RwLock;

use keke_auth_api::CredentialOrigin;
use keke_auth_api::CredentialRef;
use keke_auth_api::CredentialStore;
use keke_auth_api::StoreError;

/// A writable store held entirely in memory.
///
/// Not `#[cfg(test)]`: auth plugins need a store to test against, and the
/// alternative — each of them growing its own fake — is how the layers drift
/// apart on the rules this crate exists to enforce.
#[derive(Debug, Default)]
pub struct MemoryStore {
    values: RwLock<BTreeMap<String, String>>,
}

impl MemoryStore {
    pub const SOURCE: &'static str = "memory";

    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn origin() -> CredentialOrigin {
        CredentialOrigin {
            source: Self::SOURCE.to_string(),
            writable: true,
        }
    }

    fn poisoned() -> StoreError {
        StoreError::Backend("in-memory credential store lock was poisoned".to_string())
    }
}

impl CredentialStore for MemoryStore {
    fn load(&self, name: &CredentialRef) -> Result<Option<String>, StoreError> {
        let values = self.values.read().map_err(|_| Self::poisoned())?;
        Ok(values.get(name.as_str()).cloned().and_then(crate::present))
    }

    fn describe(&self, name: &CredentialRef) -> Result<Option<CredentialOrigin>, StoreError> {
        Ok(self.load(name)?.map(|_| Self::origin()))
    }

    fn save(&self, name: &CredentialRef, value: &str) -> Result<(), StoreError> {
        let mut values = self.values.write().map_err(|_| Self::poisoned())?;
        values.insert(name.to_string(), value.to_string());
        Ok(())
    }

    fn delete(&self, name: &CredentialRef) -> Result<bool, StoreError> {
        let mut values = self.values.write().map_err(|_| Self::poisoned())?;
        Ok(values.remove(name.as_str()).is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_saved_value_reads_back_as_absent() {
        let store = MemoryStore::new();
        let name = CredentialRef::new("XAI_API_KEY").unwrap();
        store.save(&name, "").unwrap();
        assert_eq!(store.load(&name).unwrap(), None);
        assert_eq!(store.describe(&name).unwrap(), None);
    }
}
