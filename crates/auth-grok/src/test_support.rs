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

use crate::GrokAuth;
use crate::GrokAuthConfig;
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
/// and none may read their real `~/.grok`. The importer is redirected rather
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

    /// An importer holding a grok CLI login at `$GROK_HOME/auth.json`.
    pub(crate) fn with_grok_cli_login(&self, body: serde_json::Value) -> Importer {
        let home = self.dir.path().join("grok-cli");
        std::fs::create_dir_all(&home).expect("mkdir");
        let path = home.join("auth.json");
        std::fs::write(&path, serde_json::to_vec_pretty(&body).expect("json")).expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        }
        self.importer().with_grok_home(home)
    }
}

pub(crate) fn xai(home: &Home, store: &Arc<MemoryStore>, config: GrokAuthConfig) -> GrokAuth {
    GrokAuth::new(store.clone(), home.auth_files(), config).with_importer(home.importer())
}

pub(crate) fn store_tokens(
    auth: &GrokAuth,
    access_token: String,
    refresh_token: Option<&str>,
    mode: AuthMode,
) {
    auth.auth_files
        .save(
            &auth.config().vendor,
            &AuthFile::from_tokens(
                mode,
                AuthTokens {
                    access_token,
                    refresh_token: refresh_token.map(str::to_string),
                    ..AuthTokens::default()
                },
            ),
        )
        .expect("save");
}

pub(crate) fn stored_tokens(auth: &GrokAuth) -> Option<AuthTokens> {
    auth.auth_files
        .load(&auth.config().vendor)
        .expect("load")
        .and_then(|file| file.tokens)
}
