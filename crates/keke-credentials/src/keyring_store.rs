use keke_auth_api::CredentialOrigin;
use keke_auth_api::CredentialRef;
use keke_auth_api::CredentialStore;
use keke_auth_api::StoreError;
use keyring::Entry;
use keyring::Error as KeyringError;

/// The platform keyring (Keychain, Secret Service, Credential Manager).
///
/// The service name is a constructor argument rather than a constant so a
/// deployment can keep two installations' credentials apart on one machine.
#[derive(Clone, Debug)]
pub struct KeyringStore {
    service: String,
}

impl KeyringStore {
    pub const SOURCE: &'static str = "keyring";

    #[must_use]
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }

    #[must_use]
    pub fn origin() -> CredentialOrigin {
        CredentialOrigin {
            source: Self::SOURCE.to_string(),
            writable: true,
        }
    }

    /// Whether this machine has a usable keyring at all.
    ///
    /// A headless Linux box, a locked login keyring, or a container without a
    /// session bus all produce a backend that fails every call. Callers use
    /// this to drop the layer instead of letting one unavailable backend fail
    /// every credential read on the box.
    #[must_use]
    pub fn available(&self) -> bool {
        // A read of a name that is never written distinguishes "no such
        // credential" (backend fine) from "no backend".
        match self.entry(".keke-probe").and_then(|entry| {
            entry.get_password().map(|_| ()).or_else(|err| match err {
                KeyringError::NoEntry => Ok(()),
                other => Err(other),
            })
        }) {
            Ok(()) => true,
            Err(err) => {
                tracing::warn!(
                    service = %self.service,
                    reason = classify(&err),
                    "platform keyring unavailable; falling back to the file layer"
                );
                false
            }
        }
    }

    fn entry(&self, name: &str) -> Result<Entry, KeyringError> {
        Entry::new(&self.service, name)
    }

    fn open(&self, name: &CredentialRef) -> Result<Entry, StoreError> {
        self.entry(name.as_str())
            .map_err(|err| StoreError::Backend(format!("keyring: {}", classify(&err))))
    }
}

/// `keyring::Error`'s `Display` includes the offending value for some variants,
/// so failures are reported by shape only.
fn classify(err: &KeyringError) -> &'static str {
    match err {
        KeyringError::PlatformFailure(_) => "platform failure",
        KeyringError::NoStorageAccess(_) => "no storage access",
        KeyringError::NoEntry => "no entry",
        KeyringError::BadEncoding(_) => "stored value is not UTF-8",
        KeyringError::TooLong(_, _) => "value too long for this backend",
        KeyringError::Invalid(_, _) => "invalid attribute",
        KeyringError::Ambiguous(_) => "several matching entries",
        _ => "unknown keyring failure",
    }
}

impl CredentialStore for KeyringStore {
    fn load(&self, name: &CredentialRef) -> Result<Option<String>, StoreError> {
        match self.open(name)?.get_password() {
            Ok(value) => Ok(crate::present(value)),
            Err(KeyringError::NoEntry) => Ok(None),
            Err(err) => Err(StoreError::Backend(format!("keyring: {}", classify(&err)))),
        }
    }

    fn describe(&self, name: &CredentialRef) -> Result<Option<CredentialOrigin>, StoreError> {
        Ok(self.load(name)?.map(|_| Self::origin()))
    }

    fn save(&self, name: &CredentialRef, value: &str) -> Result<(), StoreError> {
        self.open(name)?
            .set_password(value)
            .map_err(|err| StoreError::Backend(format!("keyring: {}", classify(&err))))
    }

    fn delete(&self, name: &CredentialRef) -> Result<bool, StoreError> {
        match self.open(name)?.delete_credential() {
            Ok(()) => Ok(true),
            Err(KeyringError::NoEntry) => Ok(false),
            Err(err) => Err(StoreError::Backend(format!("keyring: {}", classify(&err)))),
        }
    }
}
