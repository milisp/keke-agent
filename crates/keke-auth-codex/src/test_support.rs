//! Stubs shared by the flow tests: a `LoginUi` that records what it was asked
//! to show, and a `Delay` that records what it was asked to wait.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use keke_auth_api::LoginUi;
use keke_credentials::AuthFile;
use keke_credentials::AuthMode;
use keke_credentials::AuthTokens;
use keke_credentials::Importer;
use keke_credentials::MemoryStore;
use keke_credentials::VendorAuthStore;
use keke_paths::AbsPath;

use crate::CodexAuth;
use crate::CodexAuthConfig;
use crate::device::Delay;

#[derive(Default)]
pub(crate) struct RecordingUi {
    browser_urls: Mutex<Vec<String>>,
    device_codes: Mutex<Vec<(String, String)>>,
    notices: Mutex<Vec<String>>,
}

impl RecordingUi {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub(crate) fn browser_urls(&self) -> Vec<String> {
        self.browser_urls.lock().unwrap().clone()
    }

    pub(crate) fn device_codes(&self) -> Vec<(String, String)> {
        self.device_codes.lock().unwrap().clone()
    }

    pub(crate) fn notices(&self) -> Vec<String> {
        self.notices.lock().unwrap().clone()
    }
}

impl LoginUi for RecordingUi {
    fn open_browser(&self, url: &str) {
        self.browser_urls.lock().unwrap().push(url.to_string());
    }

    fn show_device_code(&self, code: &str, verification_uri: &str) {
        self.device_codes
            .lock()
            .unwrap()
            .push((code.to_string(), verification_uri.to_string()));
    }

    fn notice(&self, message: &str) {
        self.notices.lock().unwrap().push(message.to_string());
    }
}

/// Records the schedule instead of living through it.
#[derive(Default)]
pub(crate) struct RecordingDelay {
    waits: Mutex<Vec<Duration>>,
}

impl RecordingDelay {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub(crate) fn waits(&self) -> Vec<Duration> {
        self.waits.lock().unwrap().clone()
    }
}

impl Delay for RecordingDelay {
    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        self.waits.lock().unwrap().push(duration);
        Box::pin(std::future::ready(()))
    }
}

/// A `$KEKE_HOME` of its own, and an importer pointed at an empty directory.
///
/// Both halves matter: no test may write into the developer's real `~/.keke`,
/// and none may read their real `~/.codex`. The importer is redirected rather
/// than disabled so the tests exercise the same code path production does.
pub(crate) struct Home {
    dir: tempfile::TempDir,
}

impl Home {
    pub(crate) fn new() -> Self {
        Self {
            dir: tempfile::tempdir().expect("tempdir"),
        }
    }

    pub(crate) fn auth_files(&self) -> VendorAuthStore {
        VendorAuthStore::new(AbsPath::new(self.dir.path().join("keke")).expect("absolute"))
    }

    /// An importer that will never find anything.
    pub(crate) fn importer(&self) -> Importer {
        Importer::default()
            .with_codex_home(self.dir.path().join("no-codex"))
            .with_grok_home(self.dir.path().join("no-grok"))
    }

    fn codex_cli_path(&self) -> std::path::PathBuf {
        self.dir.path().join("codex-cli").join("auth.json")
    }

    /// An importer holding a codex CLI login at `$CODEX_HOME/auth.json`.
    pub(crate) fn with_codex_cli_login(&self, body: serde_json::Value) -> Importer {
        let path = self.codex_cli_path();
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, serde_json::to_vec_pretty(&body).expect("json")).expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        }
        self.importer()
            .with_codex_home(path.parent().expect("parent"))
    }

    /// The codex CLI file's exact bytes, so a test can pin that keke never
    /// touched them.
    pub(crate) fn codex_cli_bytes(&self) -> Vec<u8> {
        std::fs::read(self.codex_cli_path()).expect("read")
    }
}

pub(crate) fn chatgpt(home: &Home, store: &Arc<MemoryStore>, config: CodexAuthConfig) -> CodexAuth {
    CodexAuth::new(store.clone(), home.auth_files(), config).with_importer(home.importer())
}

pub(crate) fn store_tokens(auth: &CodexAuth, access_token: String, refresh_token: Option<&str>) {
    auth.auth_files
        .save(
            &auth.config().vendor,
            &AuthFile::from_tokens(
                AuthMode::Chatgpt,
                AuthTokens {
                    access_token,
                    refresh_token: refresh_token.map(str::to_string),
                    ..AuthTokens::default()
                },
            ),
        )
        .expect("save");
}

pub(crate) fn stored_tokens(auth: &CodexAuth) -> Option<AuthTokens> {
    auth.auth_files
        .load(&auth.config().vendor)
        .expect("load")
        .and_then(|file| file.tokens)
}
