//! The authentication seam.
//!
//! This crate is trait-only by design. Vendor auth plugins, the credential
//! store, and the HTTP layer all depend on it; it depends on none of them, so
//! nothing has to pull in the engine merely to attach a bearer token.
//!
//! Two rules the implementations must honor:
//!
//! * **Resolve per operation, never cache across operations.** A refreshed token
//!   must reach the next request without a restart, which only holds if callers
//!   re-read the credential each time rather than snapshotting it at startup.
//! * **An empty stored value is absent everywhere.** A blank must never
//!   masquerade as a configured secret — [`CredentialStore::load`] returns
//!   `None` for it and [`AuthProvider::has_usable_credential`] reports false.

mod store;

pub use store::CredentialOrigin;
pub use store::CredentialRef;
pub use store::CredentialStore;
pub use store::StoreError;

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// A boxed future, used because these traits are always held as `dyn`.
pub type AuthFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Why an authentication operation failed.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("no credential configured for `{0}`")]
    NotConfigured(String),
    #[error("the stored credential expired and could not be refreshed: {0}")]
    RefreshFailed(String),
    #[error("the login flow was cancelled")]
    Cancelled,
    #[error("the provider rejected the credential: {0}")]
    Rejected(String),
    #[error("credential storage failed: {0}")]
    Store(#[from] StoreError),
    #[error("{0}")]
    Other(String),
}

/// Non-secret facts about the current credential.
///
/// Deliberately carries no token. Identifiers derived from a secret are
/// namespaced UUIDv5 digests rather than the secret itself, so a snapshot is
/// always safe to log.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CredentialSnapshot {
    /// Which [`AuthProvider`] produced this.
    pub auth_id: String,
    /// How the credential was obtained, e.g. `"oauth"`, `"device-code"`, `"env"`.
    pub source: String,
    pub account_id: Option<String>,
    pub organization_id: Option<String>,
    /// Unix seconds at which the credential expires, when it is time-limited.
    pub expires_at: Option<i64>,
}

/// How a login flow reaches the person sitting in front of the terminal.
///
/// Providers never touch the terminal themselves: the host supplies this, so the
/// identical flow works in the TUI, headless, and from an editor over ACP.
pub trait LoginUi: Send + Sync {
    /// Ask the host to open `url`. The host may decline (headless), in which
    /// case it must have shown the URL some other way.
    fn open_browser(&self, url: &str);

    /// Display a device code the person must enter at `verification_uri`.
    fn show_device_code(&self, code: &str, verification_uri: &str);

    /// Report progress while the flow waits, e.g. "waiting for authorization".
    fn notice(&self, _message: &str) {}
}

/// How a request is authenticated.
///
/// Kept as an opaque header set rather than a `reqwest::RequestBuilder` so this
/// crate needs no HTTP dependency and remains usable from a test harness.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuthHeaders(Vec<(String, String)>);

impl AuthHeaders {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.0.push((name.into(), value.into()));
        self
    }

    /// A standard `Authorization: Bearer` header.
    #[must_use]
    pub fn bearer(token: &str) -> Self {
        Self::new().with("authorization", format!("Bearer {token}"))
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }
}

/// A vendor's authentication implementation.
pub trait AuthProvider: Send + Sync + 'static {
    /// Stable registry key, e.g. `"chatgpt"` or `"grok"`.
    fn id(&self) -> &'static str;

    /// Non-secret description of the current credential.
    fn snapshot(&self) -> CredentialSnapshot;

    /// Whether a credential is present and non-empty.
    fn has_usable_credential(&self) -> bool {
        true
    }

    /// Produce headers for one outbound request.
    ///
    /// Called per request, not cached, so a refresh takes effect immediately.
    fn headers(&self) -> AuthFuture<'_, Result<AuthHeaders, AuthError>>;

    /// Run the interactive login flow.
    fn login<'a>(&'a self, ui: Arc<dyn LoginUi>) -> AuthFuture<'a, Result<(), AuthError>>;

    /// Refresh after a 401.
    ///
    /// Implementations must single-flight this: several concurrent requests
    /// failing at once should produce one refresh, not one per request.
    fn refresh_after_unauthorized(&self) -> AuthFuture<'_, bool>;

    /// Discard the stored credential.
    fn logout(&self) -> AuthFuture<'_, Result<(), AuthError>>;
}

/// The set of registered auth providers.
#[derive(Default)]
pub struct AuthRegistry {
    providers: std::collections::BTreeMap<&'static str, Arc<dyn AuthProvider>>,
}

impl AuthRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `provider` under its declared id, replacing any previous entry.
    pub fn register(&mut self, provider: Arc<dyn AuthProvider>) {
        self.providers.insert(provider.id(), provider);
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn AuthProvider>> {
        self.providers.get(id).cloned()
    }

    pub fn ids(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.providers.keys().copied()
    }
}

impl fmt::Debug for AuthRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthRegistry")
            .field("ids", &self.providers.keys().collect::<Vec<_>>())
            .finish()
    }
}
