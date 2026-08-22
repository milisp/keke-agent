//! Layered credential storage: OS keyring over file over environment.
//!
//! The layering exists so a credential can be supplied the way a deployment
//! already supplies secrets — a keychain on a laptop, a file in `$KEKE_HOME` on
//! a build box, an environment variable in CI — without any of them being
//! special-cased by the code that reads it.
//!
//! Two rules from `keke_auth_api::store` are enforced here rather than left to
//! each layer: an empty value is absent everywhere (see [`present`]), and a
//! write into a layer that a read-only layer already shadows is rejected rather
//! than silently lost (see [`LayeredStore::save`]).

mod env;
mod file;
mod keyring_store;
mod layered;
mod memory;

pub use env::EnvStore;
pub use file::FileStore;
pub use file::keke_home;
pub use keyring_store::KeyringStore;
pub use layered::LayeredStore;
pub use layered::standard_store;
pub use memory::MemoryStore;

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
