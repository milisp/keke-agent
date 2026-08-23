use std::time::Duration;

use keke_auth_api::CredentialRef;
use keke_credentials::Vendor;

/// xAI's own issuer, used when a deployment does not name one.
pub const DEFAULT_ISSUER: &str = "https://auth.x.ai";
/// The public client id of the xAI CLI OAuth2 application.
pub const DEFAULT_CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
/// The vendor slug, and therefore the auth file name: `auth.grok.json`.
pub const DEFAULT_VENDOR: &str = "grok";
/// The long-lived API key alternative to a login.
pub const DEFAULT_API_KEY_REF: &str = "XAI_API_KEY";

/// The client version the subscription proxy gates on.
///
/// Not keke's version: `cli-chat-proxy.grok.com` refuses a request whose
/// `x-grok-client-version` is older than the release it currently requires,
/// with `426 Your Grok CLI version (none) is outdated`. It is the protocol
/// generation this crate speaks, and a deployment that meets a raised gate
/// before keke ships a new release needs to say so without forking the plugin.
pub const DEFAULT_CLIENT_VERSION: &str = "0.1.202";

/// What keke calls itself to that proxy. The gate is on the version alone —
/// this is honest attribution, not a passport, so keke does not pose as
/// another client.
pub const CLIENT_IDENTIFIER: &str = "keke";

const DEFAULT_SCOPES: &[&str] = &["openid", "profile", "email", "offline_access", "api:access"];

/// Everything about the xAI auth flow a deployment might reasonably change.
///
/// These are constructor arguments rather than constants because an
/// installation pointed at a private issuer, or registered as its own OAuth2
/// client, must not have to fork the plugin to say so. The `Default` impl is
/// the public xAI deployment and nothing more.
#[derive(Clone, Debug)]
pub struct GrokAuthConfig {
    pub issuer: String,
    pub client_id: String,
    pub scopes: Vec<String>,
    pub authorize_endpoint: String,
    pub token_endpoint: String,
    pub device_authorization_endpoint: String,
    /// Which `auth.<vendor>.json` the OAuth2 credential is kept in.
    pub vendor: Vendor,
    /// Reference for the API-key fallback.
    pub api_key_ref: CredentialRef,
    /// How far ahead of `exp` a token counts as expired. Without a margin a
    /// token that passes the check can still expire in flight.
    pub refresh_leeway: Duration,
    /// How long an interactive login may wait for the person to finish.
    pub login_timeout: Duration,
    /// Skip the loopback attempt. Set by a caller that knows no browser can
    /// reach this machine — an SSH session, a container, CI.
    pub device_code_only: bool,
    /// Added to the poll interval each time the issuer answers `slow_down`.
    pub slow_down_increment: Duration,
    /// Sent as `x-grok-client-version` — see [`DEFAULT_CLIENT_VERSION`].
    pub client_version: String,
}

impl GrokAuthConfig {
    /// Configure against `issuer` for `client_id`, deriving the standard
    /// endpoint paths. Override the endpoint fields afterwards for an issuer
    /// that lays them out differently.
    pub fn new(issuer: impl Into<String>, client_id: impl Into<String>) -> Self {
        let issuer = issuer.into();
        let base = issuer.trim_end_matches('/').to_string();
        Self {
            client_id: client_id.into(),
            scopes: DEFAULT_SCOPES.iter().map(|s| (*s).to_string()).collect(),
            authorize_endpoint: format!("{base}/oauth2/authorize"),
            token_endpoint: format!("{base}/oauth2/token"),
            device_authorization_endpoint: format!("{base}/oauth2/device/code"),
            issuer,
            vendor: fixed_vendor(DEFAULT_VENDOR),
            api_key_ref: fixed_ref(DEFAULT_API_KEY_REF),
            refresh_leeway: Duration::from_secs(60),
            login_timeout: Duration::from_secs(300),
            device_code_only: false,
            slow_down_increment: Duration::from_secs(5),
            client_version: DEFAULT_CLIENT_VERSION.to_string(),
        }
    }

    #[must_use]
    pub fn with_scopes(mut self, scopes: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.scopes = scopes.into_iter().map(Into::into).collect();
        self
    }

    #[must_use]
    pub fn device_code_only(mut self, only: bool) -> Self {
        self.device_code_only = only;
        self
    }

    pub(crate) fn scope_param(&self) -> String {
        self.scopes.join(" ")
    }

    fn base(&self) -> &str {
        self.issuer.trim_end_matches('/')
    }

    /// What [`Self::new`] would have derived, which is how a caller tells an
    /// endpoint a deployment stated from one this crate guessed — see
    /// [`crate::discovery`].
    pub(crate) fn derived_authorize_endpoint(&self) -> String {
        format!("{}/oauth2/authorize", self.base())
    }

    pub(crate) fn derived_token_endpoint(&self) -> String {
        format!("{}/oauth2/token", self.base())
    }

    pub(crate) fn derived_device_authorization_endpoint(&self) -> String {
        format!("{}/oauth2/device/code", self.base())
    }
}

impl Default for GrokAuthConfig {
    fn default() -> Self {
        Self::new(DEFAULT_ISSUER, DEFAULT_CLIENT_ID)
    }
}

/// The default reference is a literal that is already a shell identifier;
/// `CredentialRef::new` has no infallible form to say so.
fn fixed_ref(name: &'static str) -> CredentialRef {
    match CredentialRef::new(name) {
        Ok(reference) => reference,
        Err(_) => unreachable!("`{name}` is a shell identifier"),
    }
}

fn fixed_vendor(name: &'static str) -> Vendor {
    match Vendor::new(name) {
        Ok(vendor) => vendor,
        Err(_) => unreachable!("`{name}` is a vendor slug"),
    }
}
