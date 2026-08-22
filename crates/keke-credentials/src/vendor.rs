//! One auth file per vendor, under `$KEKE_HOME`.
//!
//! A single flat document keyed by credential name cannot hold an OAuth token
//! set without turning it into an opaque blob, and every vendor's refresh then
//! contends for the same file. One `auth.<vendor>.json` per vendor gives each
//! flow its own lock, its own atomic write, and a shape a person can read —
//! deliberately the shape codex writes, so somebody who knows one recognizes
//! the other.

use std::fs;

use chrono::DateTime;
use chrono::Utc;
use keke_paths::AbsPath;
use serde::Deserialize;
use serde::Serialize;

use crate::atomic::Staged;
use crate::error::AuthFileError;
use crate::lock::MutationLock;

/// The version this build writes, and the highest it will read.
pub const SCHEMA_VERSION: u32 = 1;

/// A credential document is a few hundred bytes; anything vastly larger is not
/// one, and reading it into memory is not worth doing to find out.
const MAX_AUTH_FILE_BYTES: u64 = 1 << 20;

/// A vendor slug, which is also part of a file name.
///
/// Validated on construction because it reaches the filesystem: a vendor named
/// `../../etc` would otherwise decide where a credential gets written.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Vendor(String);

impl Vendor {
    pub fn new(name: impl Into<String>) -> Result<Self, AuthFileError> {
        let name = name.into();
        let valid = !name.is_empty()
            && name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if valid {
            Ok(Self(name))
        } else {
            Err(AuthFileError::InvalidVendor(name))
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn file_name(&self) -> String {
        format!("auth.{}.json", self.0)
    }

    #[must_use]
    fn lock_file_name(&self) -> String {
        format!("auth.{}.lock", self.0)
    }
}

impl std::fmt::Display for Vendor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// How the credential in an auth file was obtained.
///
/// The discriminator, as in codex's `auth.json` (`"auth_mode": "chatgpt"`) and
/// the grok CLI's (`"auth_mode": "oidc"`). Reading either file is then a matter
/// of recognizing a value that is already spelled the same way.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub enum AuthMode {
    /// A long-lived key the deployment supplied.
    #[default]
    #[serde(rename = "apikey")]
    ApiKey,
    /// OpenAI's ChatGPT OAuth login.
    #[serde(rename = "chatgpt")]
    Chatgpt,
    /// An OIDC / OAuth2 login against an issuer.
    #[serde(rename = "oidc")]
    Oidc,
    /// RFC 8628 device authorization grant.
    #[serde(rename = "device-code")]
    DeviceCode,
}

impl AuthMode {
    /// The slug a [`keke_auth_api::CredentialSnapshot`] reports as its source.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ApiKey => "apikey",
            Self::Chatgpt => "chatgpt",
            Self::Oidc => "oidc",
            Self::DeviceCode => "device-code",
        }
    }
}

impl std::fmt::Display for AuthMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An OAuth token set.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct AuthTokens {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    /// Unix seconds, recorded from `expires_in` for issuers whose access token
    /// is opaque and therefore carries no readable `exp`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
}

impl AuthTokens {
    #[must_use]
    pub fn bearer(access_token: impl Into<String>) -> Self {
        Self {
            access_token: access_token.into(),
            ..Self::default()
        }
    }

    /// An empty access token is absent, per the rule the whole crate enforces.
    fn present(self) -> Option<Self> {
        (!self.access_token.trim().is_empty()).then_some(self)
    }
}

/// The contents of one `auth.<vendor>.json`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct AuthFile {
    pub schema_version: u32,
    pub auth_mode: AuthMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<AuthTokens>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_refresh: Option<DateTime<Utc>>,
}

impl AuthFile {
    /// An OAuth credential obtained through `mode`.
    #[must_use]
    pub fn from_tokens(mode: AuthMode, tokens: AuthTokens) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            auth_mode: mode,
            tokens: Some(tokens),
            api_key: None,
            last_refresh: Some(Utc::now()),
        }
    }

    /// A long-lived key.
    #[must_use]
    pub fn from_api_key(api_key: impl Into<String>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            auth_mode: AuthMode::ApiKey,
            tokens: None,
            api_key: Some(api_key.into()),
            last_refresh: None,
        }
    }

    /// Whether anything usable is in here. A blank token or key is absent.
    #[must_use]
    pub fn has_credential(&self) -> bool {
        self.tokens.is_some() || self.api_key.is_some()
    }

    /// Drop blanks so no layer can decide a stray newline is a secret.
    fn normalized(mut self) -> Self {
        self.tokens = self.tokens.and_then(AuthTokens::present);
        self.api_key = self
            .api_key
            .and_then(|key| (!key.trim().is_empty()).then_some(key));
        self
    }
}

/// The per-vendor auth files in one directory.
///
/// Holds a directory rather than a path per vendor so a caller composes it once
/// and every vendor added later lands in the same place with the same rules.
#[derive(Clone, Debug)]
pub struct VendorAuthStore {
    home: AbsPath,
}

impl VendorAuthStore {
    #[must_use]
    pub fn new(home: AbsPath) -> Self {
        Self { home }
    }

    /// The store under `$KEKE_HOME`, else `~/.keke`.
    pub fn discover() -> Result<Self, AuthFileError> {
        Ok(Self::new(crate::keke_home()?))
    }

    #[must_use]
    pub fn home(&self) -> &AbsPath {
        &self.home
    }

    /// Where `vendor`'s credential lives, whether or not it exists.
    pub fn path(&self, vendor: &Vendor) -> Result<AbsPath, AuthFileError> {
        AbsPath::new(self.home.as_path().join(vendor.file_name()))
            .map_err(|err| AuthFileError::Backend(err.to_string()))
    }

    /// Read `vendor`'s credential.
    ///
    /// `None` covers both "never logged in" and "logged in and then the
    /// credential was emptied"; a file whose permissions are wider than `0600`
    /// is an error rather than `None`, because silently ignoring it would send
    /// the person through a login flow without ever saying why.
    pub fn load(&self, vendor: &Vendor) -> Result<Option<AuthFile>, AuthFileError> {
        let path = self.path(vendor)?;
        let Some(text) = read_private(&path)? else {
            return Ok(None);
        };
        if text.trim().is_empty() {
            return Ok(None);
        }

        let file: AuthFile = serde_json::from_str(&text)
            .map_err(|err| AuthFileError::malformed(path.as_path(), &err))?;
        if file.schema_version > SCHEMA_VERSION {
            return Err(AuthFileError::UnsupportedSchema {
                path: path.to_string(),
                found: file.schema_version,
                supported: SCHEMA_VERSION,
            });
        }
        Ok(Some(file.normalized()))
    }

    /// Replace `vendor`'s credential, holding the mutation lock.
    pub fn save(&self, vendor: &Vendor, file: &AuthFile) -> Result<(), AuthFileError> {
        let _lock = self.lock(vendor)?;
        self.write(vendor, file)
    }

    /// Remove `vendor`'s credential, reporting whether there was one.
    pub fn delete(&self, vendor: &Vendor) -> Result<bool, AuthFileError> {
        let _lock = self.lock(vendor)?;
        let path = self.path(vendor)?;
        match fs::remove_file(path.as_path()) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(AuthFileError::io(path.as_path(), &err)),
        }
    }

    /// Read, transform, and write back under one lock.
    ///
    /// The lock spans the read as well as the write: a refresh that read before
    /// taking it would spend a refresh token another process had already
    /// rotated away.
    pub fn update<T>(
        &self,
        vendor: &Vendor,
        change: impl FnOnce(Option<AuthFile>) -> Result<(AuthFile, T), AuthFileError>,
    ) -> Result<T, AuthFileError> {
        let _lock = self.lock(vendor)?;
        let (file, out) = change(self.load(vendor)?)?;
        self.write(vendor, &file)?;
        Ok(out)
    }

    fn lock(&self, vendor: &Vendor) -> Result<MutationLock, AuthFileError> {
        MutationLock::acquire(self.home.as_path().join(vendor.lock_file_name()))
    }

    fn write(&self, vendor: &Vendor, file: &AuthFile) -> Result<(), AuthFileError> {
        let path = self.path(vendor)?;
        let mut file = file.clone().normalized();
        file.schema_version = SCHEMA_VERSION;
        let body = serde_json::to_vec_pretty(&file)
            .map_err(|err| AuthFileError::malformed(path.as_path(), &err))?;
        Staged::stage(path.as_path(), &body)
            .and_then(|staged| staged.commit(path.as_path()))
            .map_err(|err| AuthFileError::io(path.as_path(), &err))
    }
}

/// Read a file that must not be readable by anyone but its owner.
pub(crate) fn read_private(path: &AbsPath) -> Result<Option<String>, AuthFileError> {
    let metadata = match fs::metadata(path.as_path()) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(AuthFileError::io(path.as_path(), &err)),
    };

    if !metadata.is_file() {
        return Err(AuthFileError::Malformed {
            path: path.to_string(),
            reason: "not a regular file".to_string(),
        });
    }
    if metadata.len() > MAX_AUTH_FILE_BYTES {
        return Err(AuthFileError::Malformed {
            path: path.to_string(),
            reason: "far larger than any credential document".to_string(),
        });
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(AuthFileError::InsecurePermissions {
                path: path.to_string(),
                mode,
            });
        }
    }

    fs::read_to_string(path.as_path())
        .map(Some)
        .map_err(|err| AuthFileError::io(path.as_path(), &err))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, VendorAuthStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = AbsPath::new(dir.path()).expect("absolute");
        (dir, VendorAuthStore::new(home))
    }

    fn codex() -> Vendor {
        Vendor::new("codex").expect("slug")
    }

    #[test]
    fn a_vendor_name_that_could_escape_the_home_is_rejected() {
        for bad in ["../etc", "Codex", "code x", "", "a/b", ".hidden"] {
            assert!(
                matches!(Vendor::new(bad), Err(AuthFileError::InvalidVendor(_))),
                "expected `{bad}` to be rejected"
            );
        }
        assert_eq!(codex().file_name(), "auth.codex.json");
    }

    #[test]
    fn a_missing_credential_is_absent_rather_than_an_error() {
        let (_dir, store) = store();
        assert_eq!(store.load(&codex()).expect("load"), None);
    }

    #[test]
    fn each_auth_mode_round_trips_through_the_file() {
        let (_dir, store) = store();
        let vendor = codex();

        for mode in [
            AuthMode::Chatgpt,
            AuthMode::Oidc,
            AuthMode::DeviceCode,
            AuthMode::ApiKey,
        ] {
            let written = if mode == AuthMode::ApiKey {
                AuthFile::from_api_key("sk-test-1")
            } else {
                AuthFile::from_tokens(
                    mode,
                    AuthTokens {
                        access_token: "access-1".into(),
                        refresh_token: Some("refresh-1".into()),
                        account_id: Some("acct-1".into()),
                        expires_at: Some(4_102_444_800),
                    },
                )
            };

            store.save(&vendor, &written).expect("save");
            let read = store.load(&vendor).expect("load").expect("present");
            assert_eq!(read, written, "{mode} did not survive the round trip");
            assert_eq!(read.schema_version, SCHEMA_VERSION);
        }
    }

    #[test]
    fn the_discriminator_is_spelled_the_way_codex_and_grok_spell_it() {
        let (_dir, store) = store();
        let vendor = codex();
        store
            .save(
                &vendor,
                &AuthFile::from_tokens(AuthMode::Chatgpt, AuthTokens::bearer("access-1")),
            )
            .expect("save");

        let raw = fs::read_to_string(store.path(&vendor).expect("path").as_path()).expect("read");
        let document: serde_json::Value = serde_json::from_str(&raw).expect("json");
        assert_eq!(document["auth_mode"], "chatgpt");
        assert_eq!(document["schema_version"], 1);
        assert!(document.get("api_key").is_none());
    }

    #[test]
    fn a_blank_token_is_not_a_credential() {
        let (_dir, store) = store();
        let vendor = codex();
        store
            .save(
                &vendor,
                &AuthFile::from_tokens(AuthMode::Oidc, AuthTokens::bearer("   ")),
            )
            .expect("save");

        let read = store.load(&vendor).expect("load").expect("present");
        assert!(!read.has_credential());
    }

    #[cfg(unix)]
    #[test]
    fn a_file_anyone_can_read_is_refused_by_name() {
        use std::os::unix::fs::PermissionsExt as _;

        let (_dir, store) = store();
        let vendor = codex();
        store
            .save(
                &vendor,
                &AuthFile::from_tokens(AuthMode::Chatgpt, AuthTokens::bearer("access-1")),
            )
            .expect("save");

        let path = store.path(&vendor).expect("path");
        fs::set_permissions(path.as_path(), fs::Permissions::from_mode(0o644)).expect("chmod");

        let err = store
            .load(&vendor)
            .expect_err("a world-readable credential must be refused");
        assert!(
            matches!(&err, AuthFileError::InsecurePermissions { mode, .. } if *mode == 0o644),
            "got {err}"
        );
        let message = err.to_string();
        assert!(message.contains(path.as_str()), "{message}");
        assert!(message.contains("chmod 600"), "{message}");
    }

    #[cfg(unix)]
    #[test]
    fn what_we_write_is_readable_only_by_its_owner() {
        use std::os::unix::fs::PermissionsExt as _;

        let (_dir, store) = store();
        let vendor = codex();
        store
            .save(&vendor, &AuthFile::from_api_key("sk-test-1"))
            .expect("save");
        let mode = fs::metadata(store.path(&vendor).expect("path").as_path())
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "mode was {mode:o}");
    }

    #[test]
    fn an_interrupted_write_leaves_the_previous_credential_intact() {
        let (_dir, store) = store();
        let vendor = codex();
        let first = AuthFile::from_tokens(AuthMode::Chatgpt, AuthTokens::bearer("access-1"));
        store.save(&vendor, &first).expect("save");

        // The staged bytes are the whole of the next write; dropping them is
        // exactly what a crash between staging and rename leaves behind.
        let path = store.path(&vendor).expect("path");
        let body = serde_json::to_vec_pretty(&AuthFile::from_tokens(
            AuthMode::Chatgpt,
            AuthTokens::bearer("access-2"),
        ))
        .expect("serialize");
        drop(Staged::stage(path.as_path(), &body).expect("stage"));

        let read = store.load(&vendor).expect("load").expect("present");
        assert_eq!(read.tokens.expect("tokens").access_token, "access-1");
    }

    #[test]
    fn a_file_from_a_newer_keke_is_refused_rather_than_misread() {
        let (_dir, store) = store();
        let vendor = codex();
        store
            .save(&vendor, &AuthFile::from_api_key("sk-test-1"))
            .expect("save");
        let path = store.path(&vendor).expect("path");
        let raw = fs::read_to_string(path.as_path()).expect("read");
        let bumped = raw.replace("\"schema_version\": 1", "\"schema_version\": 99");
        fs::write(path.as_path(), bumped).expect("write");

        assert!(matches!(
            store.load(&vendor),
            Err(AuthFileError::UnsupportedSchema { found: 99, .. })
        ));
    }

    #[test]
    fn delete_reports_what_it_removed() {
        let (_dir, store) = store();
        let vendor = codex();
        store
            .save(&vendor, &AuthFile::from_api_key("sk-test-1"))
            .expect("save");
        assert!(store.delete(&vendor).expect("delete"));
        assert!(!store.delete(&vendor).expect("delete"));
    }

    #[test]
    fn two_processes_mutating_at_once_do_not_lose_a_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = AbsPath::new(dir.path()).expect("absolute");
        let vendor = codex();
        VendorAuthStore::new(home.clone())
            .save(&vendor, &AuthFile::from_api_key("0"))
            .expect("save");

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let store = VendorAuthStore::new(home.clone());
                let vendor = vendor.clone();
                std::thread::spawn(move || {
                    store
                        .update(&vendor, |current| {
                            let count: u32 = current
                                .and_then(|file| file.api_key)
                                .and_then(|key| key.parse().ok())
                                .unwrap_or(0);
                            Ok((AuthFile::from_api_key((count + 1).to_string()), ()))
                        })
                        .expect("update");
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("thread");
        }

        let final_count = VendorAuthStore::new(home)
            .load(&vendor)
            .expect("load")
            .expect("present")
            .api_key
            .expect("key");
        assert_eq!(
            final_count, "8",
            "a mutation was lost to an interleaved write"
        );
    }
}
