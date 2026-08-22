//! The interface's implementation of [`keke_auth_api::LoginUi`].
//!
//! A provider that printed a device code to stdout would scribble over the
//! alternate screen and leave the person with a corrupted display and no code.
//! So the flow sends a [`Notice`] instead and the draw loop puts it in the
//! transcript, where it scrolls and can be re-read.

use std::fmt;

use keke_auth_api::LoginUi;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::UnboundedSender;

/// Something a login flow needs the person to see.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notice {
    /// The host could not open a browser, or chose not to; the URL is shown so
    /// it can be copied.
    OpenBrowser(String),
    DeviceCode {
        code: String,
        verification_uri: String,
    },
    Message(String),
}

impl fmt::Display for Notice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Notice::OpenBrowser(url) => write!(f, "sign in at {url}"),
            Notice::DeviceCode {
                code,
                verification_uri,
            } => write!(f, "enter code {code} at {verification_uri}"),
            Notice::Message(message) => f.write_str(message),
        }
    }
}

/// A [`LoginUi`] that renders inside the interface rather than over it.
pub struct TuiLoginUi {
    notices: UnboundedSender<Notice>,
}

impl TuiLoginUi {
    /// Pair a login UI with the stream `run_with_login` drains.
    pub fn new() -> (Self, UnboundedReceiver<Notice>) {
        let (notices, receiver) = tokio::sync::mpsc::unbounded_channel();
        (Self { notices }, receiver)
    }
}

impl LoginUi for TuiLoginUi {
    /// Never launches a browser: the alternate screen owns the terminal, and a
    /// spawned browser that steals focus mid-turn is worse than a URL to copy.
    fn open_browser(&self, url: &str) {
        let _ = self.notices.send(Notice::OpenBrowser(url.to_string()));
    }

    fn show_device_code(&self, code: &str, verification_uri: &str) {
        let _ = self.notices.send(Notice::DeviceCode {
            code: code.to_string(),
            verification_uri: verification_uri.to_string(),
        });
    }

    fn notice(&self, message: &str) {
        let _ = self.notices.send(Notice::Message(message.to_string()));
    }
}
