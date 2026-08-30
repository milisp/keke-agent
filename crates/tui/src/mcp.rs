//! What the interface knows about MCP servers, which is deliberately little.
//!
//! The status rows are computed by the host and handed over as text: the
//! interface has no business knowing what a transport is, where a token is
//! kept, or how trust is decided. Signing in is the same — it is a capability
//! the host implements, because it owns the credential store.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use keke_auth_api::LoginUi;

/// One server, as a person needs to see it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpServerStatus {
    /// What the server is called, and what `/mcp login` takes.
    pub name: String,
    /// Which plugin or directory configured it.
    pub plugin: String,
    /// One line saying how it is reached — a command line, or a URL.
    pub transport: String,
    /// Whether reaching it means a URL rather than a program here. Only a
    /// remote server has anything to sign in to.
    pub remote: bool,
    /// Whether a token is stored for it. Meaningless for a local server, and
    /// not shown for one.
    pub signed_in: bool,
    /// Whether keke will actually reach it, or is holding it back for want of
    /// trust.
    pub allowed: bool,
    /// Whether a person has left this one running. `false` means configured
    /// but disabled — still listed, just never started.
    pub enabled: bool,
}

/// Signing in to a remote server.
///
/// A trait rather than a direct call because the credential store belongs to
/// the host: the interface knows only that a name can be signed in and that it
/// takes a while.
pub trait McpSignIn: Send + Sync + 'static {
    fn sign_in(
        &self,
        name: String,
        ui: Arc<dyn LoginUi>,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;
}

/// Changing what is configured, from inside the interface.
///
/// A trait rather than a direct call for the same reason as [`McpSignIn`]: the
/// `.mcp.json` a server lives in belongs to the host, which knows the scopes
/// and the trust store. The interface only ever gets a name and a verdict.
pub trait McpManage: Send + Sync + 'static {
    /// Flip whether `name` starts. Returns the row's new state.
    fn set_disabled(&self, name: &str, disabled: bool) -> Result<(), String>;
    /// Drop `name` from whichever file configured it.
    fn remove(&self, name: &str) -> Result<(), String>;
    /// Re-read every server from disk, picking up an edit made outside the
    /// overlay — by hand, by `keke mcp add`, or by a plugin that just landed.
    fn refresh(&self) -> Result<Vec<McpServerStatus>, String>;
}

/// Why `/mcp` has nothing to open.
///
/// The overlay lists what is configured; this is the other case, where the
/// answer is not a list at all but a command to run.
#[must_use]
pub fn nothing_configured() -> String {
    "no MCP servers configured\n  \
     add one with `keke mcp add --transport http <name> <url>`"
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_servers_is_not_an_empty_screen() {
        assert!(nothing_configured().contains("keke mcp add"));
    }
}
