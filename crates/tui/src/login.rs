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
    /// A server's token was just stored. Carries the name rather than only
    /// prose because the status list has to stop saying "not signed in".
    SignedIn(String),
    /// How one MCP server's login is going.
    ///
    /// Named rather than anonymous prose because this belongs on that server's
    /// row: a login someone started from the overlay is a thing happening to a
    /// row they are looking at, not a remark in the conversation.
    McpProgress {
        name: String,
        message: String,
    },
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
            Notice::SignedIn(name) => write!(f, "signed in to `{name}`"),
            Notice::McpProgress { name, message } => write!(f, "`{name}`: {message}"),
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

/// Send progress to a stream someone else already owns.
///
/// A flow the interface starts itself — `/mcp login` — reports into the same
/// channel the startup login uses, so there is one path to the transcript
/// rather than two that could render differently.
impl From<UnboundedSender<Notice>> for TuiLoginUi {
    fn from(notices: UnboundedSender<Notice>) -> Self {
        Self { notices }
    }
}

/// A [`LoginUi`] whose progress lands on one MCP server's row.
///
/// The same flow as [`TuiLoginUi`] — it opens the same browser — but everything
/// it has to say is tagged with the server it is about, so the overlay can show
/// it in place instead of the transcript growing a line per step for something
/// the person is already watching.
pub(crate) struct McpLoginUi {
    name: String,
    notices: UnboundedSender<Notice>,
}

impl McpLoginUi {
    #[must_use]
    pub(crate) fn new(name: String, notices: UnboundedSender<Notice>) -> Self {
        Self { name, notices }
    }

    fn progress(&self, message: String) {
        let _ = self.notices.send(Notice::McpProgress {
            name: self.name.clone(),
            message,
        });
    }
}

impl LoginUi for McpLoginUi {
    fn open_browser(&self, url: &str) {
        // The URL is reported even when the browser opens: a spawn that fails
        // says nothing, and on a headless box there is nothing to open at all.
        self.progress(format!("sign in at {url}"));
        #[cfg(not(test))]
        let _ = keke_oauth::open_in_browser(url);
    }

    fn show_device_code(&self, code: &str, verification_uri: &str) {
        self.progress(format!("enter code {code} at {verification_uri}"));
    }

    fn notice(&self, message: &str) {
        self.progress(message.to_string());
    }
}

impl LoginUi for TuiLoginUi {
    /// Opens a browser *and* sends the URL to the transcript.
    ///
    /// Spawning writes nothing to the terminal, so it is safe under the
    /// alternate screen; printing is not, which is why the URL goes to the
    /// transcript rather than to stdout. The notice is not a fallback — a
    /// headless box or a failed spawn says nothing, so the person must have the
    /// URL either way.
    fn open_browser(&self, url: &str) {
        let _ = self.notices.send(Notice::OpenBrowser(url.to_string()));
        // A test asserting the notice must not pop a real browser open.
        #[cfg(not(test))]
        let _ = keke_oauth::open_in_browser(url);
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
