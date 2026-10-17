//! Stubs for login-flow tests: a `LoginUi` that records what it was asked to
//! show, and a `Delay` that records what it was asked to wait.
//!
//! Test code, so a poisoned lock is a failed test rather than something to
//! recover from.

#![allow(clippy::unwrap_used)]

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use keke_auth_api::LoginUi;

use crate::device::Delay;

#[derive(Default)]
pub struct RecordingUi {
    browser_urls: Mutex<Vec<String>>,
    device_codes: Mutex<Vec<(String, String)>>,
    notices: Mutex<Vec<String>>,
}

impl RecordingUi {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn browser_urls(&self) -> Vec<String> {
        self.browser_urls.lock().unwrap().clone()
    }

    pub fn device_codes(&self) -> Vec<(String, String)> {
        self.device_codes.lock().unwrap().clone()
    }

    pub fn notices(&self) -> Vec<String> {
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
pub struct RecordingDelay {
    waits: Mutex<Vec<Duration>>,
}

impl RecordingDelay {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn waits(&self) -> Vec<Duration> {
        self.waits.lock().unwrap().clone()
    }
}

impl Delay for RecordingDelay {
    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        self.waits.lock().unwrap().push(duration);
        Box::pin(std::future::ready(()))
    }
}
