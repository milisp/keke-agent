//! The terminal implementation of the host capabilities a session needs.

use std::io::IsTerminal;
use std::io::Write;

use keke_auth_api::LoginUi;
use keke_oauth::open_in_browser;

/// Drives a login flow from a terminal.
///
/// The provider never touches the terminal itself — it calls these — which is
/// what lets the identical flow run under a TUI or over a protocol later.
pub(crate) struct TerminalLoginUi;

impl LoginUi for TerminalLoginUi {
    fn open_browser(&self, url: &str) {
        // Print first, then try. If opening fails, or there is no browser to
        // open, the person still has the URL — which is the whole point.
        println!("Opening your browser to authenticate:\n  {url}\n");
        let _ = open_in_browser(url);
    }

    fn show_device_code(&self, code: &str, verification_uri: &str) {
        println!("To authenticate, visit:\n  {verification_uri}\nand enter the code:\n  {code}\n");
        let _ = std::io::stdout().flush();
    }

    fn notice(&self, message: &str) {
        println!("{message}");
        let _ = std::io::stdout().flush();
    }
}

/// Drives a login flow from behind an ACP connection.
///
/// stdout is the protocol wire there — writing a notice to it the way
/// [`TerminalLoginUi`] does would corrupt the stream a client is parsing as
/// JSON-RPC, so this reports to stderr instead.
pub(crate) struct AcpLoginUi;

impl LoginUi for AcpLoginUi {
    fn open_browser(&self, url: &str) {
        eprintln!("Opening your browser to authenticate:\n  {url}\n");
        let _ = open_in_browser(url);
    }

    fn show_device_code(&self, code: &str, verification_uri: &str) {
        eprintln!("To authenticate, visit:\n  {verification_uri}\nand enter the code:\n  {code}\n");
    }

    fn notice(&self, message: &str) {
        eprintln!("{message}");
    }
}

/// Whether output is going to a terminal rather than a pipe.
///
/// Used to decide between streaming prose and emitting a clean final answer:
/// piping `keke exec` into another program should not interleave progress.
#[must_use]
pub(crate) fn is_interactive() -> bool {
    std::io::stdout().is_terminal()
}
