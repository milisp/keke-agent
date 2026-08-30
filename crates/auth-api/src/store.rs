//! Credential storage.
//!
//! Configuration files carry *references* to secrets — never values. A
//! [`CredentialRef`] is a shell-identifier-shaped name like `XAI_API_KEY`; the
//! store owns the value and where it lives. That separation is what lets a
//! settings surface describe a credential without ever seeing it.

use std::fmt;

use serde::Deserialize;
use serde::Serialize;

/// Why a store operation failed.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("invalid credential reference `{0}`: expected a shell identifier")]
    InvalidRef(String),
    /// The reference resolves from a source this store cannot write, so a write
    /// would appear to succeed while reads kept returning the shadowing value.
    #[error("`{name}` is shadowed by read-only source `{origin}` and cannot be written")]
    Shadowed { name: String, origin: String },
    #[error("credential backend failure: {0}")]
    Backend(String),
}

/// A name identifying a credential, not the credential itself.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CredentialRef(String);

impl CredentialRef {
    /// Validate and wrap a reference name.
    ///
    /// The shape is a POSIX shell identifier so a reference can always be
    /// satisfied from the environment as a last resort.
    pub fn new(name: impl Into<String>) -> Result<Self, StoreError> {
        let name = name.into();
        let mut chars = name.chars();
        let valid = match chars.next() {
            Some(first) if first.is_ascii_alphabetic() || first == '_' => {
                chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
            }
            _ => false,
        };
        if valid {
            Ok(Self(name))
        } else {
            Err(StoreError::InvalidRef(name))
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CredentialRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for CredentialRef {
    type Error = StoreError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<CredentialRef> for String {
    fn from(value: CredentialRef) -> Self {
        value.0
    }
}

/// Where a resolved credential came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialOrigin {
    /// A stable slug, e.g. `"keyring"`, `"env"`, `"file"`.
    pub source: String,
    /// Whether this source accepts writes.
    pub writable: bool,
}

/// A backing store for credential values.
///
/// Implementations are layered (keyring over file over environment); the layer
/// that resolves a reference first wins, and a write into a lower layer while a
/// read-only higher layer shadows it is rejected rather than silently lost.
pub trait CredentialStore: Send + Sync {
    /// Read a value. Returns `None` when absent **or empty** — a blank never
    /// counts as configured.
    fn load(&self, name: &CredentialRef) -> Result<Option<String>, StoreError>;

    /// Describe where `name` would resolve from, without reading its value.
    fn describe(&self, name: &CredentialRef) -> Result<Option<CredentialOrigin>, StoreError>;

    /// Write a value.
    fn save(&self, name: &CredentialRef, value: &str) -> Result<(), StoreError>;

    /// Remove a value. Returns whether anything was removed.
    fn delete(&self, name: &CredentialRef) -> Result<bool, StoreError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_shell_identifiers() {
        assert!(CredentialRef::new("XAI_API_KEY").is_ok());
        assert!(CredentialRef::new("_private1").is_ok());
    }

    #[test]
    fn rejects_non_identifiers() {
        for bad in ["", "1LEADING", "has-dash", "has space", "has.dot"] {
            assert!(
                matches!(CredentialRef::new(bad), Err(StoreError::InvalidRef(_))),
                "expected `{bad}` to be rejected"
            );
        }
    }
}
