//! Failures specific to the per-vendor auth files.
//!
//! A separate type from [`StoreError`] because the failures worth *matching on*
//! here — an auth file anyone can read, a file from a newer keke — have no
//! `StoreError` variant, and `StoreError` lives in a tier 0 contract crate this
//! crate is not allowed to change. Conversion into `StoreError::Backend`
//! preserves the message so a plugin surfacing an `AuthError` still names the
//! file and the fix.

use std::path::Path;

use keke_auth_api::StoreError;

/// Why reading or writing an `auth.<vendor>.json` failed.
#[derive(Debug, thiserror::Error)]
pub enum AuthFileError {
    #[error("invalid vendor name `{0}`: expected lowercase letters, digits, and dashes")]
    InvalidVendor(String),

    /// The file is readable or writable by somebody other than its owner.
    ///
    /// Refusing rather than repairing: a credential that has already been
    /// exposed should be re-minted, and silently tightening the mode would hide
    /// that it ever was.
    #[error(
        "{path} is accessible to other users (mode {mode:04o}); \
         run `chmod 600 {path}` and log in again"
    )]
    InsecurePermissions { path: String, mode: u32 },

    #[error("{path} is not a keke auth file: {reason}")]
    Malformed { path: String, reason: String },

    #[error(
        "{path} was written by a newer keke (schema version {found}; \
         this build understands {supported})"
    )]
    UnsupportedSchema {
        path: String,
        found: u32,
        supported: u32,
    },

    #[error("another keke process is holding {path} (waited {millis}ms)")]
    Locked { path: String, millis: u64 },

    #[error("{path}: {message}")]
    Io { path: String, message: String },

    #[error("{0}")]
    Backend(String),
}

impl AuthFileError {
    pub(crate) fn io(path: impl AsRef<Path>, err: &std::io::Error) -> Self {
        Self::Io {
            path: path.as_ref().display().to_string(),
            message: err.to_string(),
        }
    }

    /// `serde_json` errors quote the offending input, so only the shape of the
    /// failure may be surfaced — the input here is a credential.
    pub(crate) fn malformed(path: impl AsRef<Path>, err: &serde_json::Error) -> Self {
        let reason = match err.classify() {
            serde_json::error::Category::Io => "read failed",
            serde_json::error::Category::Syntax => "malformed JSON",
            serde_json::error::Category::Data => "unexpected shape",
            serde_json::error::Category::Eof => "truncated",
        };
        Self::Malformed {
            path: path.as_ref().display().to_string(),
            reason: reason.to_string(),
        }
    }
}

impl From<AuthFileError> for StoreError {
    fn from(err: AuthFileError) -> Self {
        StoreError::Backend(err.to_string())
    }
}

impl From<StoreError> for AuthFileError {
    fn from(err: StoreError) -> Self {
        AuthFileError::Backend(err.to_string())
    }
}
