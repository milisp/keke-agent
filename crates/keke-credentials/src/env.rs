use keke_auth_api::CredentialOrigin;
use keke_auth_api::CredentialRef;
use keke_auth_api::CredentialStore;
use keke_auth_api::StoreError;

/// The process environment, read-only.
///
/// It is the last layer by design: `CredentialRef` is constrained to a shell
/// identifier precisely so that any reference can still be satisfied here when
/// nothing else holds it.
#[derive(Clone, Copy, Debug, Default)]
pub struct EnvStore;

impl EnvStore {
    pub const SOURCE: &'static str = "env";

    #[must_use]
    pub fn origin() -> CredentialOrigin {
        CredentialOrigin {
            source: Self::SOURCE.to_string(),
            writable: false,
        }
    }

    fn refuse(name: &CredentialRef) -> StoreError {
        StoreError::Shadowed {
            name: name.to_string(),
            origin: Self::SOURCE.to_string(),
        }
    }
}

impl CredentialStore for EnvStore {
    fn load(&self, name: &CredentialRef) -> Result<Option<String>, StoreError> {
        Ok(std::env::var(name.as_str()).ok().and_then(crate::present))
    }

    fn describe(&self, name: &CredentialRef) -> Result<Option<CredentialOrigin>, StoreError> {
        Ok(self.load(name)?.map(|_| Self::origin()))
    }

    /// Always refused: this process cannot change its parent's environment, so
    /// a write that appeared to succeed would be a lie.
    fn save(&self, name: &CredentialRef, _value: &str) -> Result<(), StoreError> {
        Err(Self::refuse(name))
    }

    fn delete(&self, name: &CredentialRef) -> Result<bool, StoreError> {
        Err(Self::refuse(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unset_variable_is_absent() {
        let name = CredentialRef::new("KEKE_TEST_ENV_DEFINITELY_UNSET").unwrap();
        assert_eq!(EnvStore.load(&name).unwrap(), None);
        assert_eq!(EnvStore.describe(&name).unwrap(), None);
    }

    #[test]
    fn the_environment_never_accepts_a_write() {
        let name = CredentialRef::new("KEKE_TEST_ENV_RO").unwrap();
        assert!(matches!(
            EnvStore.save(&name, "v"),
            Err(StoreError::Shadowed { .. })
        ));
        assert!(matches!(
            EnvStore.delete(&name),
            Err(StoreError::Shadowed { .. })
        ));
    }
}
