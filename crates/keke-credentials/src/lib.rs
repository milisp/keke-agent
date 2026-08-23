//! Credential storage.
//!
//! Two surfaces, because two things are being stored:
//!
//! * [`CredentialStore`](keke_auth_api::CredentialStore) — a name to a string,
//!   layered keyring over file over environment. This is how a deployment's
//!   long-lived API keys resolve (`XAI_API_KEY`, `NVIDIA_API_KEY`), and the
//!   environment layer is why a `CredentialRef` is shaped like a shell
//!   identifier.
//! * [`VendorAuthStore`] — one `auth.<vendor>.json` per vendor, holding an
//!   OAuth token set with its mode, its expiry, and its own mutation lock. A
//!   token set does not fit the first surface without becoming an opaque blob
//!   under a single key, and every vendor's refresh would then contend for one
//!   file.
//!
//! An `keke-auth-*` plugin uses both: the vendor store for anything a login
//! minted, the `CredentialStore` for an api key the deployment supplied.

mod atomic;
mod env;
mod error;
mod file;
mod import;
mod keyring_store;
mod layered;
mod lock;
mod memory;
mod vendor;

pub use env::EnvStore;
pub use error::AuthFileError;
pub use file::FileStore;
pub use file::keke_home;
pub use import::ImportedCredential;
pub use import::Importer;
pub use import::Provenance;
pub use import::import;
pub use keyring_store::KeyringStore;
pub use layered::LayeredStore;
pub use layered::StoreMode;
pub use layered::standard_store;
pub use layered::standard_store_with_mode;
pub use memory::MemoryStore;
pub use vendor::AuthFile;
pub use vendor::AuthMode;
pub use vendor::AuthTokens;
pub use vendor::Mutation;
pub use vendor::SCHEMA_VERSION;
pub use vendor::Vendor;
pub use vendor::VendorAuthStore;

/// Apply the "an empty stored value is absent" rule.
///
/// Every layer funnels its reads through this, because the rule only holds if
/// no layer can forget it: a blank that reached one backend but not another
/// would make the same reference look configured from one machine and not from
/// the next. Whitespace counts as blank — a credential accidentally set to a
/// stray newline is a misconfiguration, not a secret.
#[must_use]
pub(crate) fn present(value: String) -> Option<String> {
    (!value.trim().is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blank_value_is_not_a_credential() {
        assert_eq!(present("k".into()), Some("k".into()));
        assert_eq!(present(String::new()), None);
        assert_eq!(present("  \n\t ".into()), None);
    }
}
