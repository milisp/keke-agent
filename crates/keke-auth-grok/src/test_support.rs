//! Stubs shared by the flow tests: a `LoginUi` that records what it was asked
//! to show, and a `Delay` that records what it was asked to wait.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use keke_auth_api::CredentialStore as _;
use keke_auth_api::LoginUi;
use keke_credentials::MemoryStore;

use crate::GrokAuth;
use crate::GrokAuthConfig;
use crate::device::Delay;
use crate::tokens::StoredTokens;

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

pub(crate) fn xai(store: &Arc<MemoryStore>, config: GrokAuthConfig) -> GrokAuth {
    GrokAuth::new(store.clone(), config)
}

pub(crate) fn store_tokens(
    store: &Arc<MemoryStore>,
    auth: &GrokAuth,
    access_token: String,
    refresh_token: Option<&str>,
    source: &str,
) {
    let tokens = StoredTokens {
        access_token,
        refresh_token: refresh_token.map(str::to_string),
        expires_at: None,
        source: source.to_string(),
    };
    store
        .save(
            &auth.config().tokens_ref,
            &serde_json::to_string(&tokens).unwrap(),
        )
        .unwrap();
}
