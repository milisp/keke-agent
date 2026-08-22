use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::Path;

use keke_auth_api::CredentialOrigin;
use keke_auth_api::CredentialRef;
use keke_auth_api::CredentialStore;
use keke_auth_api::StoreError;
use keke_paths::AbsPath;

const FILE_NAME: &str = "credentials.json";

/// Resolve `$KEKE_HOME`, falling back to `~/.keke`.
///
/// Kept here rather than in `keke-paths` because the fallback is a policy
/// choice, and this is the lowest crate that has to make it.
pub fn keke_home() -> Result<AbsPath, StoreError> {
    let raw = match std::env::var("KEKE_HOME") {
        Ok(value) if !value.trim().is_empty() => std::path::PathBuf::from(value),
        _ => dirs::home_dir()
            .ok_or_else(|| StoreError::Backend("no home directory to resolve $KEKE_HOME".into()))?
            .join(".keke"),
    };
    AbsPath::new(raw).map_err(|err| StoreError::Backend(err.to_string()))
}

/// A JSON object of credentials in `$KEKE_HOME/credentials.json`.
///
/// Written through a temporary file in the same directory so a crash mid-write
/// cannot leave a half-parsed document where every credential looks absent, and
/// created `0600` before the rename so the secret is never briefly world
/// readable.
#[derive(Clone, Debug)]
pub struct FileStore {
    path: AbsPath,
}

impl FileStore {
    pub const SOURCE: &'static str = "file";

    #[must_use]
    pub fn new(path: AbsPath) -> Self {
        Self { path }
    }

    /// The store at `<home>/credentials.json`.
    pub fn in_home(home: &AbsPath) -> Result<Self, StoreError> {
        AbsPath::new(home.as_path().join(FILE_NAME))
            .map(Self::new)
            .map_err(|err| StoreError::Backend(err.to_string()))
    }

    /// The store under [`keke_home`].
    pub fn discover() -> Result<Self, StoreError> {
        Self::in_home(&keke_home()?)
    }

    #[must_use]
    pub fn origin() -> CredentialOrigin {
        CredentialOrigin {
            source: Self::SOURCE.to_string(),
            writable: true,
        }
    }

    #[must_use]
    pub fn path(&self) -> &AbsPath {
        &self.path
    }

    fn read(&self) -> Result<BTreeMap<String, String>, StoreError> {
        match fs::read_to_string(self.path.as_path()) {
            Ok(text) if text.trim().is_empty() => Ok(BTreeMap::new()),
            Ok(text) => serde_json::from_str(&text).map_err(|err| {
                // The path, never the contents: the parse error would quote the
                // document, and the document is secrets.
                StoreError::Backend(format!(
                    "{} is not a credential document: {}",
                    self.path,
                    err.classify_for_log()
                ))
            }),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(err) => Err(StoreError::Backend(format!("{}: {err}", self.path))),
        }
    }

    fn write(&self, values: &BTreeMap<String, String>) -> Result<(), StoreError> {
        let path = self.path.as_path();
        let dir = path
            .parent()
            .ok_or_else(|| StoreError::Backend(format!("{} has no parent directory", self.path)))?;
        fs::create_dir_all(dir).map_err(|err| StoreError::Backend(format!("{dir:?}: {err}")))?;

        let temp = dir.join(format!(".{FILE_NAME}.tmp"));
        let body = serde_json::to_vec_pretty(values)
            .map_err(|err| StoreError::Backend(err.to_string()))?;
        write_private(&temp, &body)
            .map_err(|err| StoreError::Backend(format!("{temp:?}: {err}")))?;
        fs::rename(&temp, path).map_err(|err| {
            let _ = fs::remove_file(&temp);
            StoreError::Backend(format!("{}: {err}", self.path))
        })
    }
}

fn write_private(path: &Path, body: &[u8]) -> std::io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(body)?;
    file.sync_all()
}

/// `serde_json` errors quote the offending input, so only the shape of the
/// failure is safe to surface.
trait ClassifyForLog {
    fn classify_for_log(&self) -> &'static str;
}

impl ClassifyForLog for serde_json::Error {
    fn classify_for_log(&self) -> &'static str {
        match self.classify() {
            serde_json::error::Category::Io => "read failed",
            serde_json::error::Category::Syntax => "malformed JSON",
            serde_json::error::Category::Data => "expected an object of string values",
            serde_json::error::Category::Eof => "truncated",
        }
    }
}

impl CredentialStore for FileStore {
    fn load(&self, name: &CredentialRef) -> Result<Option<String>, StoreError> {
        Ok(self.read()?.remove(name.as_str()).and_then(crate::present))
    }

    fn describe(&self, name: &CredentialRef) -> Result<Option<CredentialOrigin>, StoreError> {
        Ok(self.load(name)?.map(|_| Self::origin()))
    }

    fn save(&self, name: &CredentialRef, value: &str) -> Result<(), StoreError> {
        let mut values = self.read()?;
        values.insert(name.to_string(), value.to_string());
        self.write(&values)
    }

    fn delete(&self, name: &CredentialRef) -> Result<bool, StoreError> {
        let mut values = self.read()?;
        if values.remove(name.as_str()).is_none() {
            return Ok(false);
        }
        self.write(&values)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(dir: &tempfile::TempDir) -> FileStore {
        FileStore::in_home(&AbsPath::new(dir.path()).unwrap()).unwrap()
    }

    #[test]
    fn a_missing_file_is_absent_rather_than_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let name = CredentialRef::new("XAI_API_KEY").unwrap();
        assert_eq!(store(&dir).load(&name).unwrap(), None);
    }

    #[test]
    fn an_empty_stored_value_reads_back_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let name = CredentialRef::new("XAI_API_KEY").unwrap();
        store.save(&name, "").unwrap();
        assert_eq!(store.load(&name).unwrap(), None);
        assert_eq!(store.describe(&name).unwrap(), None);
    }

    #[test]
    fn a_round_trip_survives_and_delete_reports_what_it_removed() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let name = CredentialRef::new("XAI_API_KEY").unwrap();
        store.save(&name, "secret").unwrap();
        assert_eq!(store.load(&name).unwrap().as_deref(), Some("secret"));
        assert!(store.delete(&name).unwrap());
        assert!(!store.delete(&name).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn the_document_is_not_readable_by_anyone_else() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        store
            .save(&CredentialRef::new("XAI_API_KEY").unwrap(), "secret")
            .unwrap();
        let mode = fs::metadata(store.path().as_path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "mode was {mode:o}");
    }
}
