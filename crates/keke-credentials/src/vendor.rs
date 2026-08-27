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
///
/// v2 holds several accounts where v1 held one. A v1 file is read as a single
/// account named [`DEFAULT_ACCOUNT`] and rewritten as v2 on the next save, so
/// nobody has to log in again for the upgrade.
pub const SCHEMA_VERSION: u32 = 2;

/// What the one account in a v1 file is called once it is read as v2.
///
/// Also what a login gets when the issuer told us nothing to name it by: an
/// account still has to be addressable, and a person with exactly one has no
/// reason to care what it is called.
pub const DEFAULT_ACCOUNT: &str = "default";

/// The account name that means "the long-lived key this deployment supplied".
///
/// A key is one more way to authenticate as a vendor, so it is an account like
/// any other — which is what lets `[providers.xai] account = "apikey"` sit
/// beside `[providers.grok]` on a login, one vendor and two identities. The
/// name matches the scope the grok CLI files its own key under, so an import
/// lands where a person would look for it.
pub const API_KEY_ACCOUNT: &str = "apikey";

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
    /// Who minted this token set, when it is known.
    ///
    /// Recorded rather than re-derived because a vendor's issuer is not a
    /// property of the *build*: a login imported from another CLI, or made
    /// against a private deployment, was signed by whoever signed it, and a
    /// refresh posted to a constant instead fails as an unreachable host or a
    /// 404 — both of which read as a revoked login rather than as the wrong
    /// address. See [`crate::Importer`], which is where the issuer used to be
    /// dropped on the floor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issuer: Option<String>,
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

/// The contents of one `auth.<vendor>.json`: every account, and which one is
/// in force.
///
/// A vendor's credential file holds more than one login because a person has
/// more than one account — work and personal, most often. Which one a session
/// authenticates as is chosen by whatever names it (a provider instance's
/// `account`, a directory override, a flag); [`Self::active`] is the fallback
/// for when nothing did, not a mode anyone is expected to set by hand.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AuthDocument {
    pub schema_version: u32,
    /// The account used when nothing names one. `None` with a single account
    /// means that one; `None` with several is an error at use, not at read —
    /// see [`AuthDocument::resolve`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,
    pub accounts: std::collections::BTreeMap<String, AuthFile>,
}

/// What was actually on disk, before migration.
///
/// Untagged because the two versions are told apart by shape rather than by a
/// discriminator: v1 was written before there was anything to discriminate on,
/// so `accounts` being present is the only honest signal.
#[derive(Deserialize)]
#[serde(untagged)]
enum StoredDocument {
    V2(AuthDocument),
    V1(StoredV1),
}

/// A v1 file: one credential, with the version beside it rather than above it.
#[derive(Deserialize)]
struct StoredV1 {
    #[serde(default)]
    schema_version: u32,
    #[serde(flatten)]
    file: AuthFile,
}

impl<'de> Deserialize<'de> for AuthDocument {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            #[serde(default)]
            schema_version: u32,
            #[serde(default)]
            active: Option<String>,
            accounts: std::collections::BTreeMap<String, AuthFile>,
        }
        let raw = Raw::deserialize(deserializer)?;
        Ok(Self {
            schema_version: raw.schema_version,
            active: raw.active,
            accounts: raw.accounts,
        })
    }
}

impl AuthDocument {
    /// One account under [`DEFAULT_ACCOUNT`], which is what a fresh login and
    /// a migrated v1 file both produce.
    #[must_use]
    pub fn single(file: AuthFile) -> Self {
        Self::with_account(DEFAULT_ACCOUNT, file)
    }

    /// One account under a name the caller knows — an email a login's claims
    /// carried, most often.
    #[must_use]
    pub fn with_account(name: impl Into<String>, file: AuthFile) -> Self {
        let name = name.into();
        Self {
            schema_version: SCHEMA_VERSION,
            accounts: std::iter::once((name.clone(), file)).collect(),
            active: Some(name),
        }
    }

    /// The account `wanted` names, or the one in force when it names none.
    ///
    /// Absence of a requested account is `None` rather than a fallback to the
    /// active one: a session that asked to be `work@corp.com` and silently got
    /// the personal login would spend the wrong quota under the wrong identity,
    /// which is worse than not running.
    #[must_use]
    pub fn resolve(&self, wanted: Option<&str>) -> Option<(&str, &AuthFile)> {
        if let Some(name) = wanted {
            return self
                .accounts
                .get_key_value(name)
                .map(|(name, file)| (name.as_str(), file));
        }
        if let Some(active) = self
            .active
            .as_deref()
            .and_then(|name| self.accounts.get_key_value(name))
        {
            return Some((active.0.as_str(), active.1));
        }
        // A file with exactly one account needs no `active` to be unambiguous.
        // Several without one is left to the caller, which is the only place
        // that can say what the person was trying to do (invariant 8).
        match self.accounts.len() {
            1 => self
                .accounts
                .iter()
                .next()
                .map(|(name, file)| (name.as_str(), file)),
            _ => None,
        }
    }

    /// Add or replace one account, leaving every other untouched.
    ///
    /// Whole-document rewrites are what a refresh must not do: two sessions
    /// renewing two accounts would each write back the copy they read, and the
    /// later write would silently revert the earlier one's rotation.
    pub fn put(&mut self, name: impl Into<String>, file: AuthFile) {
        let name = name.into();
        if self.active.is_none() {
            self.active = Some(name.clone());
        }
        self.accounts.insert(name, file);
    }

    /// The accounts holding something usable, in a stable order.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.accounts
            .iter()
            .filter(|(_, file)| file.has_credential())
            .map(|(name, _)| name.as_str())
            .collect()
    }

    fn normalized(mut self) -> Self {
        self.accounts = self
            .accounts
            .into_iter()
            .map(|(name, file)| (name, file.normalized()))
            .collect();
        self
    }
}

/// One account's credential.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct AuthFile {
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

/// The mutation lock on one vendor's credential, held until dropped.
///
/// Read the credential through [`Self::load`] *after* taking this, never
/// before: a refresh that read first would spend a refresh token another
/// process has since rotated away, and an issuer that rotates answers a
/// superseded token with `invalid_grant` — indistinguishable, to the person
/// reading the error, from a revoked login.
#[derive(Debug)]
pub struct Mutation<'a> {
    store: &'a VendorAuthStore,
    vendor: Vendor,
    /// Which account this mutation is for. A refresh reads and writes one
    /// account, so holding the name here is what keeps it from writing back a
    /// whole document and reverting another session's rotation.
    account: Option<String>,
    _lock: MutationLock,
}

impl Mutation<'_> {
    /// The stored credential as it is right now, under the lock.
    pub fn load(&self) -> Result<Option<AuthFile>, AuthFileError> {
        self.store
            .load_account(&self.vendor, self.account.as_deref())
    }

    /// Replace it, still under the lock, leaving every other account alone.
    pub fn save(&self, file: &AuthFile) -> Result<(), AuthFileError> {
        self.store
            .put_account(&self.vendor, self.account.as_deref(), file)
    }

    /// Which account this mutation resolved to, once it is known.
    #[must_use]
    pub fn account(&self) -> Option<&str> {
        self.account.as_deref()
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
        self.load_account(vendor, None)
    }

    /// Read the account `wanted` names, or the one in force when it names none.
    pub fn load_account(
        &self,
        vendor: &Vendor,
        wanted: Option<&str>,
    ) -> Result<Option<AuthFile>, AuthFileError> {
        Ok(self
            .document(vendor)?
            .and_then(|document| document.resolve(wanted).map(|(_, file)| file.clone())))
    }

    /// Every account in `vendor`'s file, and which is in force.
    pub fn document(&self, vendor: &Vendor) -> Result<Option<AuthDocument>, AuthFileError> {
        let path = self.path(vendor)?;
        let Some(text) = read_private(&path)? else {
            return Ok(None);
        };
        if text.trim().is_empty() {
            return Ok(None);
        }

        let stored: StoredDocument = serde_json::from_str(&text)
            .map_err(|err| AuthFileError::malformed(path.as_path(), &err))?;
        // A v1 file is one unnamed account. Migrating on read rather than
        // demanding a fresh login is the whole reason the version is stored.
        let document = match stored {
            StoredDocument::V2(document) => document,
            // The version travels with the migration: a v1 file from a build
            // newer than this one is still refused, not quietly accepted
            // because migrating it happened to reset the number.
            StoredDocument::V1(v1) => AuthDocument {
                schema_version: v1.schema_version,
                ..AuthDocument::single(v1.file)
            },
        };
        if document.schema_version > SCHEMA_VERSION {
            return Err(AuthFileError::UnsupportedSchema {
                path: path.to_string(),
                found: document.schema_version,
                supported: SCHEMA_VERSION,
            });
        }
        Ok(Some(document.normalized()))
    }

    /// Replace the credential of the account in force, holding the mutation
    /// lock and leaving every other account untouched.
    pub fn save(&self, vendor: &Vendor, file: &AuthFile) -> Result<(), AuthFileError> {
        self.save_account(vendor, None, file)
    }

    /// Replace one named account, leaving every other untouched.
    ///
    /// `None` writes the account in force, or starts the file off with
    /// [`DEFAULT_ACCOUNT`] when there is none yet.
    pub fn save_account(
        &self,
        vendor: &Vendor,
        name: Option<&str>,
        file: &AuthFile,
    ) -> Result<(), AuthFileError> {
        let _lock = self.lock(vendor)?;
        self.put_account(vendor, name, file)
    }

    /// Make `name` the account used when nothing else says.
    ///
    /// An account that is not there is refused rather than recorded: an
    /// `active` pointing at nothing would resolve to nothing, and the person
    /// would be told they are not logged in when they are.
    pub fn set_active(&self, vendor: &Vendor, name: &str) -> Result<(), AuthFileError> {
        let _lock = self.lock(vendor)?;
        let mut document = self
            .document(vendor)?
            .ok_or_else(|| AuthFileError::UnknownAccount {
                vendor: vendor.to_string(),
                account: name.to_string(),
            })?;
        if !document.accounts.contains_key(name) {
            return Err(AuthFileError::UnknownAccount {
                vendor: vendor.to_string(),
                account: name.to_string(),
            });
        }
        document.active = Some(name.to_string());
        self.write_document(vendor, &document)
    }

    /// Merge one account into the stored document. Caller holds the lock.
    fn put_account(
        &self,
        vendor: &Vendor,
        name: Option<&str>,
        file: &AuthFile,
    ) -> Result<(), AuthFileError> {
        let mut document = self.document(vendor)?.unwrap_or(AuthDocument {
            schema_version: SCHEMA_VERSION,
            active: None,
            accounts: std::collections::BTreeMap::new(),
        });
        let name = name
            .map(str::to_string)
            .or_else(|| document.active.clone())
            .unwrap_or_else(|| DEFAULT_ACCOUNT.to_string());
        document.schema_version = SCHEMA_VERSION;
        document.put(name, file.clone());
        self.write_document(vendor, &document)
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

    /// Take the mutation lock and hold it across an `await`.
    ///
    /// [`Self::update`] cannot span a token request: its closure is
    /// synchronous, and a refresh has to read the stored credential, ask the
    /// issuer, and write the answer back without another process rotating the
    /// refresh token in between. This hands the lock out instead, so the whole
    /// exchange happens under it.
    pub fn begin(&self, vendor: &Vendor) -> Result<Mutation<'_>, AuthFileError> {
        self.begin_account(vendor, None)
    }

    /// [`Self::begin`] for one named account.
    pub fn begin_account(
        &self,
        vendor: &Vendor,
        account: Option<&str>,
    ) -> Result<Mutation<'_>, AuthFileError> {
        Ok(Mutation {
            store: self,
            vendor: vendor.clone(),
            account: account.map(str::to_string),
            _lock: self.lock(vendor)?,
        })
    }

    fn lock(&self, vendor: &Vendor) -> Result<MutationLock, AuthFileError> {
        MutationLock::acquire(self.home.as_path().join(vendor.lock_file_name()))
    }

    /// Merge one account into the file. Never a whole-document overwrite —
    /// see [`AuthDocument::put`] for why a refresh must not do that.
    fn write(&self, vendor: &Vendor, file: &AuthFile) -> Result<(), AuthFileError> {
        self.put_account(vendor, None, file)
    }

    fn write_document(
        &self,
        vendor: &Vendor,
        document: &AuthDocument,
    ) -> Result<(), AuthFileError> {
        let path = self.path(vendor)?;
        let mut document = document.clone().normalized();
        document.schema_version = SCHEMA_VERSION;
        let body = serde_json::to_vec_pretty(&document)
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

    /// A fixture has to be `0600` or the store refuses it, which is the point
    /// of the check and not something a test gets to skip.
    fn set_private(path: &AbsPath) {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path.as_path(), fs::Permissions::from_mode(0o600)).expect("chmod");
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
                        issuer: None,
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
        let account = &document["accounts"][DEFAULT_ACCOUNT];
        assert_eq!(account["auth_mode"], "chatgpt");
        assert_eq!(document["schema_version"], 2);
        assert!(account.get("api_key").is_none());
    }

    /// The v1 file an earlier keke wrote is one account, not a reason to make
    /// somebody log in again.
    #[test]
    fn a_v1_file_is_read_as_a_single_account() {
        let (_dir, store) = store();
        let vendor = codex();
        let path = store.path(&vendor).expect("path");
        fs::write(
            path.as_path(),
            serde_json::json!({
                "schema_version": 1,
                "auth_mode": "chatgpt",
                "tokens": { "access_token": "access-v1" },
            })
            .to_string(),
        )
        .expect("write");
        set_private(&path);

        let document = store.document(&vendor).expect("load").expect("present");
        assert_eq!(document.names(), vec![DEFAULT_ACCOUNT]);
        let (name, file) = document.resolve(None).expect("an account in force");
        assert_eq!(name, DEFAULT_ACCOUNT);
        assert_eq!(
            file.tokens.as_ref().expect("tokens").access_token,
            "access-v1"
        );
    }

    /// Two sessions renewing two accounts must not revert each other: a write
    /// touches one account and leaves the rest of the document as it found it.
    #[test]
    fn saving_one_account_leaves_the_others_untouched() {
        let (_dir, store) = store();
        let vendor = codex();
        store
            .save_account(
                &vendor,
                Some("work@corp.com"),
                &AuthFile::from_tokens(AuthMode::Chatgpt, AuthTokens::bearer("work-1")),
            )
            .expect("save work");
        store
            .save_account(
                &vendor,
                Some("me@home.com"),
                &AuthFile::from_tokens(AuthMode::Chatgpt, AuthTokens::bearer("home-1")),
            )
            .expect("save home");

        store
            .save_account(
                &vendor,
                Some("work@corp.com"),
                &AuthFile::from_tokens(AuthMode::Chatgpt, AuthTokens::bearer("work-2")),
            )
            .expect("refresh work");

        let document = store.document(&vendor).expect("load").expect("present");
        assert_eq!(document.names(), vec!["me@home.com", "work@corp.com"]);
        assert_eq!(
            document.accounts["me@home.com"]
                .tokens
                .as_ref()
                .expect("tokens")
                .access_token,
            "home-1",
            "refreshing one account must not revert another"
        );
        assert_eq!(
            document.accounts["work@corp.com"]
                .tokens
                .as_ref()
                .expect("tokens")
                .access_token,
            "work-2"
        );
    }

    /// Asking to be one account and silently getting another would spend the
    /// wrong quota under the wrong identity.
    #[test]
    fn an_account_that_is_not_stored_does_not_fall_back_to_the_active_one() {
        let (_dir, store) = store();
        let vendor = codex();
        store
            .save_account(
                &vendor,
                Some("me@home.com"),
                &AuthFile::from_tokens(AuthMode::Chatgpt, AuthTokens::bearer("home-1")),
            )
            .expect("save");

        assert!(
            store
                .load_account(&vendor, Some("work@corp.com"))
                .expect("load")
                .is_none()
        );
        assert!(
            store.load_account(&vendor, None).expect("load").is_some(),
            "the account in force still resolves"
        );
    }

    /// An `active` pointing at nothing would report a logged-in person as
    /// logged out.
    #[test]
    fn an_account_that_is_not_stored_cannot_be_made_active() {
        let (_dir, store) = store();
        let vendor = codex();
        store
            .save_account(
                &vendor,
                Some("me@home.com"),
                &AuthFile::from_tokens(AuthMode::Chatgpt, AuthTokens::bearer("home-1")),
            )
            .expect("save");

        assert!(matches!(
            store.set_active(&vendor, "nobody@nowhere.com"),
            Err(AuthFileError::UnknownAccount { .. })
        ));
        store
            .set_active(&vendor, "me@home.com")
            .expect("an account that is stored can be made active");
    }

    /// Who signed a token set survives the round trip, so a refresh goes back
    /// to that issuer rather than to whatever this build's constant says.
    #[test]
    fn the_issuer_that_minted_a_credential_is_stored_with_it() {
        let (_dir, store) = store();
        let vendor = codex();
        let mut tokens = AuthTokens::bearer("access-1");
        tokens.issuer = Some("https://auth.private.example".to_string());
        store
            .save(&vendor, &AuthFile::from_tokens(AuthMode::Oidc, tokens))
            .expect("save");

        let read = store.load(&vendor).expect("load").expect("present");
        assert_eq!(
            read.tokens.expect("tokens").issuer.as_deref(),
            Some("https://auth.private.example")
        );
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
        let bumped = raw.replace("\"schema_version\": 2", "\"schema_version\": 99");
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
