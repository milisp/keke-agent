//! Adopting a login another CLI already performed.
//!
//! Somebody who has run `codex login` or `grok login` on this machine has
//! already proved who they are to the same issuer keke would send them to.
//! Making them do it again is a worse experience for no security gain, so keke
//! reads those files — and only reads them.
//!
//! # Precedence
//!
//! An explicit `keke login` result wins over an import; an import wins over
//! nothing. Concretely: `auth.<vendor>.json` under `$KEKE_HOME` is consulted
//! first and answers whenever it holds a credential, and only its absence lets
//! an imported credential be used. That ordering is what makes `keke login`
//! meaningful — a person who deliberately logs in as a second account must not
//! find the first tool's account still in force.
//!
//! Importing never writes to another tool's file. keke does not own those
//! files, does not know what else reads them, and must never be the reason one
//! of them is truncated or has its refresh token rotated out from under its
//! owner.

use std::path::PathBuf;

use chrono::DateTime;
use chrono::Utc;
use keke_paths::AbsPath;
use serde::Deserialize;

use crate::error::AuthFileError;
use crate::vendor::AuthDocument;
use crate::vendor::AuthFile;
use crate::vendor::AuthMode;
use crate::vendor::AuthTokens;
use crate::vendor::Vendor;
use crate::vendor::read_private;

/// Where an imported credential came from, in terms a surface can show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provenance {
    /// The tool whose login this is, e.g. `"codex"`.
    pub tool: &'static str,
    /// The file it was read from.
    pub path: AbsPath,
    /// The environment variable that named the directory, when one did.
    pub home_var: Option<&'static str>,
}

impl std::fmt::Display for Provenance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the {} CLI login at {}", self.tool, self.path)
    }
}

/// A credential found in another tool's file, converted to keke's shape.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportedCredential {
    pub vendor: Vendor,
    /// Ready to be written as `auth.<vendor>.json` — but not written by the
    /// import itself.
    ///
    /// Every account the foreign tool held, not the newest one. The grok CLI
    /// keys its `auth.json` by `"<issuer>::<client_id>"` and a person with two
    /// logins has two records; collapsing them to one discarded both the other
    /// account and the issuer that signed it.
    pub auth: AuthDocument,
    pub provenance: Provenance,
}

impl ImportedCredential {
    /// The account the foreign tool had in force, when it had one.
    ///
    /// `None` is reachable only for a document with several accounts and no
    /// `active` — which an import never produces, but which the type permits
    /// and a caller must therefore be allowed to see rather than be panicked
    /// on.
    #[must_use]
    pub fn active_account(&self) -> Option<&AuthFile> {
        self.auth.resolve(None).map(|(_, file)| file)
    }
}

/// Which foreign CLIs an [`Importer`] knows how to read.
///
/// The homes are fields rather than reads of `$CODEX_HOME` at the point of use
/// so a test can point at a fixture. Mutating the process environment to
/// redirect a test is both `unsafe` in this edition and shared state between
/// parallel tests; the real reason this is a struct is that neither is
/// acceptable for a test that must never touch the real `~/.codex`.
#[derive(Clone, Debug, Default)]
pub struct Importer {
    codex_home: Option<PathBuf>,
    grok_home: Option<PathBuf>,
}

impl Importer {
    /// Resolve each tool's home the way that tool does.
    #[must_use]
    pub fn from_env() -> Self {
        // Another tool's login is shared machine state, exactly as the OS
        // keyring is: a test that reads it passes or fails depending on who is
        // signed in on the machine, and a person may simply not want keke using
        // a credential they granted to a different tool.
        if matches!(std::env::var("KEKE_IMPORT").as_deref(), Ok("off")) {
            return Self::disabled();
        }
        Self {
            codex_home: tool_home("CODEX_HOME", ".codex"),
            grok_home: tool_home("GROK_HOME", ".grok"),
        }
    }

    /// An importer that finds nothing, whatever is on disk.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            codex_home: None,
            grok_home: None,
        }
    }

    #[must_use]
    pub fn with_codex_home(mut self, home: impl Into<PathBuf>) -> Self {
        self.codex_home = Some(home.into());
        self
    }

    #[must_use]
    pub fn with_grok_home(mut self, home: impl Into<PathBuf>) -> Self {
        self.grok_home = Some(home.into());
        self
    }

    /// Look for a login `vendor` could adopt.
    ///
    /// `Ok(None)` means nothing was found — including for a vendor no foreign
    /// CLI corresponds to. An error means a file was there and could not be
    /// trusted; callers running a login flow should report it and carry on
    /// rather than refuse to log in.
    pub fn import(&self, vendor: &Vendor) -> Result<Option<ImportedCredential>, AuthFileError> {
        match vendor.as_str() {
            "codex" => self.import_codex(vendor),
            "grok" => self.import_grok(vendor),
            _ => Ok(None),
        }
    }

    fn import_codex(&self, vendor: &Vendor) -> Result<Option<ImportedCredential>, AuthFileError> {
        let Some((path, home_var)) = self.locate(self.codex_home.as_ref(), "CODEX_HOME")? else {
            return Ok(None);
        };
        let Some(text) = read_private(&path)? else {
            return Ok(None);
        };

        let document: CodexAuthJson = serde_json::from_str(&text)
            .map_err(|err| AuthFileError::malformed(path.as_path(), &err))?;
        let auth = document.into_auth_file();

        // codex's own file holds exactly one credential, so an import from it
        // is one account. Naming it `default` rather than inventing an
        // identity keeps the migration honest about what the file said.
        Ok(auth.map(|auth| ImportedCredential {
            vendor: vendor.clone(),
            auth: AuthDocument::single(auth),
            provenance: Provenance {
                tool: "codex",
                path,
                home_var,
            },
        }))
    }

    fn import_grok(&self, vendor: &Vendor) -> Result<Option<ImportedCredential>, AuthFileError> {
        let Some((path, home_var)) = self.locate(self.grok_home.as_ref(), "GROK_HOME")? else {
            return Ok(None);
        };
        let Some(text) = read_private(&path)? else {
            return Ok(None);
        };

        // The grok CLI keys its auth.json by scope, one record per issuer plus
        // one for a plain API key, so picking a credential means choosing among
        // them rather than deserializing a single record.
        let store: std::collections::BTreeMap<String, GrokRecord> = serde_json::from_str(&text)
            .map_err(|err| AuthFileError::malformed(path.as_path(), &err))?;
        let Some(auth) = pick_grok_credential(store) else {
            return Ok(None);
        };

        Ok(Some(ImportedCredential {
            vendor: vendor.clone(),
            auth,
            provenance: Provenance {
                tool: "grok",
                path,
                home_var,
            },
        }))
    }

    /// The `auth.json` under `home`, and whether an environment variable chose
    /// the directory.
    fn locate(
        &self,
        home: Option<&PathBuf>,
        var: &'static str,
    ) -> Result<Option<(AbsPath, Option<&'static str>)>, AuthFileError> {
        let Some(home) = home else {
            return Ok(None);
        };
        let path = AbsPath::new(home.join("auth.json"))
            .map_err(|err| AuthFileError::Backend(err.to_string()))?;
        let from_env = std::env::var_os(var).is_some_and(|value| !value.is_empty());
        Ok(Some((path, from_env.then_some(var))))
    }
}

/// Read a login another CLI already performed, resolving that CLI's home the
/// way it does. See the module documentation for the precedence rule.
pub fn import(vendor: &Vendor) -> Result<Option<ImportedCredential>, AuthFileError> {
    Importer::from_env().import(vendor)
}

fn tool_home(var: &'static str, dir: &str) -> Option<PathBuf> {
    match std::env::var(var) {
        Ok(value) if !value.trim().is_empty() => Some(PathBuf::from(value)),
        _ => dirs::home_dir().map(|home| home.join(dir)),
    }
}

/// The subset of `$CODEX_HOME/auth.json` keke can act on.
///
/// Deliberately partial: codex's document also carries agent identities and
/// Bedrock keys, none of which keke knows how to use, and refusing to parse a
/// file because it holds a field we ignore would be a bad reason to make
/// somebody log in again.
#[derive(Debug, Deserialize)]
struct CodexAuthJson {
    #[serde(rename = "OPENAI_API_KEY")]
    openai_api_key: Option<String>,
    #[serde(default)]
    tokens: Option<CodexTokens>,
    #[serde(default)]
    last_refresh: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize)]
struct CodexTokens {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    account_id: Option<String>,
}

impl CodexAuthJson {
    fn into_auth_file(self) -> Option<AuthFile> {
        if let Some(tokens) = self
            .tokens
            .filter(|tokens| !tokens.access_token.trim().is_empty())
        {
            return Some(AuthFile {
                auth_mode: AuthMode::Chatgpt,
                tokens: Some(AuthTokens {
                    access_token: tokens.access_token,
                    refresh_token: tokens.refresh_token,
                    account_id: tokens.account_id,
                    issuer: None,
                    expires_at: None,
                }),
                api_key: None,
                last_refresh: self.last_refresh,
            });
        }
        self.openai_api_key
            .filter(|key| !key.trim().is_empty())
            .map(AuthFile::from_api_key)
    }
}

/// The grok CLI's per-scope record in `$GROK_HOME/auth.json`.
#[derive(Debug, Deserialize)]
struct GrokRecord {
    #[serde(default)]
    key: String,
    /// Who signed this token set. The grok CLI records it per record because
    /// one file can hold logins from several issuers; keke keeps it for the
    /// same reason, and because a refresh posted to a constant instead is the
    /// bug this field exists to prevent.
    #[serde(default)]
    oidc_issuer: Option<String>,
    #[serde(default)]
    auth_mode: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    create_time: Option<DateTime<Utc>>,
    #[serde(default)]
    user_id: Option<String>,
}

/// The scope the grok CLI files a plain API key under.
const GROK_API_KEY_SCOPE: &str = "xai::api_key";

/// Prefer a real login over a stored API key, and the newest of several logins.
///
/// A person with both has logged in interactively at some point; that is the
/// credential with an identity attached, and the one whose expiry keke can
/// renew.
fn pick_grok_credential(
    store: std::collections::BTreeMap<String, GrokRecord>,
) -> Option<AuthDocument> {
    let (logins, keys): (Vec<_>, Vec<_>) = store
        .into_iter()
        .filter(|(_, record)| !record.key.trim().is_empty())
        .partition(|(scope, record)| {
            scope != GROK_API_KEY_SCOPE && record.auth_mode.as_deref() != Some("api_key")
        });

    // Newest last, so it is the one `put` leaves active after the loop.
    let mut logins = logins;
    logins.sort_by_key(|(_, record)| record.create_time);

    let mut document = AuthDocument {
        schema_version: crate::vendor::SCHEMA_VERSION,
        active: None,
        accounts: std::collections::BTreeMap::new(),
    };
    for (scope, record) in logins {
        let issuer = record.oidc_issuer.clone().or_else(|| issuer_of(&scope));
        let name = record
            .user_id
            .clone()
            .unwrap_or_else(|| scope_account_name(&scope));
        let file = AuthFile {
            auth_mode: AuthMode::Oidc,
            tokens: Some(AuthTokens {
                access_token: record.key,
                refresh_token: record.refresh_token,
                account_id: record.user_id,
                issuer,
                expires_at: record.expires_at.map(|at| at.timestamp()),
            }),
            api_key: None,
            last_refresh: record.create_time,
        };
        document.accounts.insert(name.clone(), file);
        document.active = Some(name);
    }

    // A stored key sits beside the logins rather than replacing them: it is
    // one more way to authenticate as this vendor, which is exactly what an
    // account is. A person with both keeps both.
    if let Some((_, record)) = keys.into_iter().next() {
        document.accounts.insert(
            crate::vendor::API_KEY_ACCOUNT.to_string(),
            AuthFile::from_api_key(record.key),
        );
        if document.active.is_none() {
            document.active = Some(crate::vendor::API_KEY_ACCOUNT.to_string());
        }
    }

    (!document.accounts.is_empty()).then_some(document)
}

/// The issuer half of a `"<issuer>::<client_id>"` scope key.
fn issuer_of(scope: &str) -> Option<String> {
    scope
        .split_once("::")
        .map(|(issuer, _)| issuer.to_string())
        .filter(|issuer| issuer.starts_with("http"))
}

/// A name for a login whose record carried no user id.
///
/// The scope is not pretty, but it is unique and it is what the person would
/// see in the foreign tool's own file — better than a counter nobody can match
/// back to anything.
fn scope_account_name(scope: &str) -> String {
    scope
        .split_once("::")
        .map_or_else(|| scope.to_string(), |(_, client)| client.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    /// A fixture written the way the tool itself writes it: `0600`.
    fn write_foreign(home: &Path, body: serde_json::Value) -> PathBuf {
        fs::create_dir_all(home).expect("mkdir");
        let path = home.join("auth.json");
        fs::write(&path, serde_json::to_vec_pretty(&body).expect("json")).expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("chmod");
        }
        path
    }

    fn codex() -> Vendor {
        Vendor::new("codex").expect("slug")
    }

    fn grok() -> Vendor {
        Vendor::new("grok").expect("slug")
    }

    #[test]
    fn a_codex_login_is_found_and_its_file_is_never_written_to() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join(".codex");
        let path = write_foreign(
            &home,
            serde_json::json!({
                "OPENAI_API_KEY": serde_json::Value::Null,
                "tokens": {
                    "id_token": "header.payload.signature",
                    "access_token": "codex-access-1",
                    "refresh_token": "codex-refresh-1",
                    "account_id": "acct-9",
                },
                "last_refresh": "2026-08-21T10:00:00Z",
            }),
        );
        let before = fs::read(&path).expect("read");

        let found = Importer::default()
            .with_codex_home(&home)
            .import(&codex())
            .expect("import")
            .expect("a codex login must be found");

        assert_eq!(
            found.active_account().expect("an account").auth_mode,
            AuthMode::Chatgpt
        );
        let tokens = found
            .active_account()
            .expect("an account")
            .tokens
            .clone()
            .expect("tokens");
        assert_eq!(tokens.access_token, "codex-access-1");
        assert_eq!(tokens.refresh_token.as_deref(), Some("codex-refresh-1"));
        assert_eq!(tokens.account_id.as_deref(), Some("acct-9"));
        assert_eq!(found.provenance.tool, "codex");
        assert!(
            found.provenance.to_string().contains("auth.json"),
            "{}",
            found.provenance
        );

        assert_eq!(
            fs::read(&path).expect("read"),
            before,
            "importing must never modify another tool's file"
        );
    }

    #[test]
    fn a_codex_api_key_login_imports_as_an_api_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join(".codex");
        write_foreign(
            &home,
            serde_json::json!({ "OPENAI_API_KEY": "sk-codex-1", "tokens": null }),
        );

        let found = Importer::default()
            .with_codex_home(&home)
            .import(&codex())
            .expect("import")
            .expect("present");
        assert_eq!(
            found.active_account().expect("an account").auth_mode,
            AuthMode::ApiKey
        );
        assert_eq!(
            found
                .active_account()
                .expect("an account")
                .api_key
                .as_deref(),
            Some("sk-codex-1")
        );
    }

    #[test]
    fn a_grok_login_is_read_from_its_scoped_document() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join(".grok");
        write_foreign(
            &home,
            serde_json::json!({
                "xai::api_key": {
                    "key": "xai-key-1",
                    "auth_mode": "api_key",
                    "create_time": "2026-01-01T00:00:00Z",
                    "user_id": "",
                },
                "https://auth.x.ai": {
                    "key": "xai-access-1",
                    "auth_mode": "oidc",
                    "create_time": "2026-08-01T00:00:00Z",
                    "user_id": "user-3",
                    "refresh_token": "xai-refresh-1",
                    "expires_at": "2026-09-01T00:00:00Z",
                },
            }),
        );

        let found = Importer::default()
            .with_grok_home(&home)
            .import(&grok())
            .expect("import")
            .expect("present");

        assert_eq!(
            found.active_account().expect("an account").auth_mode,
            AuthMode::Oidc
        );
        let tokens = found
            .active_account()
            .expect("an account")
            .tokens
            .clone()
            .expect("tokens");
        assert_eq!(tokens.access_token, "xai-access-1");
        assert_eq!(tokens.refresh_token.as_deref(), Some("xai-refresh-1"));
        assert_eq!(tokens.account_id.as_deref(), Some("user-3"));
        assert_eq!(found.provenance.tool, "grok");
    }

    #[test]
    fn a_grok_api_key_is_used_when_there_is_no_login() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join(".grok");
        write_foreign(
            &home,
            serde_json::json!({
                "xai::api_key": { "key": "xai-key-1", "auth_mode": "api_key", "user_id": "" },
            }),
        );

        let found = Importer::default()
            .with_grok_home(&home)
            .import(&grok())
            .expect("import")
            .expect("present");
        assert_eq!(
            found.active_account().expect("an account").auth_mode,
            AuthMode::ApiKey
        );
        assert_eq!(
            found
                .active_account()
                .expect("an account")
                .api_key
                .as_deref(),
            Some("xai-key-1")
        );
    }

    #[test]
    fn nothing_to_import_is_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let importer = Importer::default()
            .with_codex_home(dir.path().join("absent"))
            .with_grok_home(dir.path().join("absent"));
        assert_eq!(importer.import(&codex()).expect("import"), None);
        assert_eq!(importer.import(&grok()).expect("import"), None);
        assert_eq!(
            importer
                .import(&Vendor::new("nvidia").expect("slug"))
                .expect("import"),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_foreign_file_anyone_can_read_is_refused_by_name() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join(".codex");
        let path = write_foreign(&home, serde_json::json!({ "OPENAI_API_KEY": "sk-1" }));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).expect("chmod");

        let err = Importer::default()
            .with_codex_home(&home)
            .import(&codex())
            .expect_err("an exposed credential must not be adopted silently");
        assert!(
            matches!(err, AuthFileError::InsecurePermissions { .. }),
            "got {err}"
        );
    }
}
