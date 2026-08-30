use std::time::Duration;

use keke_auth_api::CredentialRef;
use keke_credentials::Vendor;

/// OpenAI's own issuer, used when a deployment does not name one.
pub const DEFAULT_ISSUER: &str = "https://auth.openai.com";
/// The public client id of the ChatGPT CLI OAuth2 application.
pub const DEFAULT_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// The vendor slug, and therefore the auth file name: `auth.codex.json`.
pub const DEFAULT_VENDOR: &str = "codex";
/// The long-lived API key alternative to a login.
pub const DEFAULT_API_KEY_REF: &str = "OPENAI_API_KEY";

/// What a login asks for. The connectors scopes are part of what this client
/// is registered for; asking for less is not a smaller request, it is a
/// different one than the registration describes.
const DEFAULT_SCOPES: &[&str] = &[
    "openid",
    "profile",
    "email",
    "offline_access",
    "api.connectors.read",
    "api.connectors.invoke",
];

/// How this client names itself to the authorize endpoint.
pub const DEFAULT_ORIGINATOR: &str = "codex_cli_rs";

/// Everything about the ChatGPT auth flow a deployment might reasonably change.
///
/// These are constructor arguments rather than constants because an
/// installation pointed at a private issuer, or registered as its own OAuth2
/// client, must not have to fork the plugin to say so. The `Default` impl is
/// the public OpenAI deployment and nothing more.
#[derive(Clone, Debug)]
pub struct CodexAuthConfig {
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
    /// The loopback port the redirect URI is registered at — see
    /// [`crate::ported::codex::authorize::DEFAULT_PORT`].
    pub callback_port: u16,
    /// Sent as `originator` — see [`DEFAULT_ORIGINATOR`].
    pub originator: String,
}

impl CodexAuthConfig {
    /// Configure against `issuer` for `client_id`, deriving the standard
    /// endpoint paths. Override the endpoint fields afterwards for an issuer
    /// that lays them out differently.
    /// The standard token path under an issuer that is not necessarily this
    /// config's — a credential records who signed it, and that is who renews
    /// it. Posting to the build's constant instead fails as an unreachable
    /// host or a 404, and both read as a revoked login rather than as the
    /// wrong address.
    pub(crate) fn token_endpoint_for(&self, issuer: Option<&str>) -> String {
        match issuer {
            // A deployment that stated an endpoint outranks both: it said
            // where the endpoint is, and neither the credential nor discovery
            // gets to overrule that.
            _ if self.token_endpoint != self.derived_token_endpoint() => {
                self.token_endpoint.clone()
            }
            Some(issuer) => format!("{}/oauth/token", issuer.trim_end_matches('/')),
            None => self.token_endpoint.clone(),
        }
    }

    fn derived_token_endpoint(&self) -> String {
        format!("{}/oauth/token", self.issuer.trim_end_matches('/'))
    }

    pub fn new(issuer: impl Into<String>, client_id: impl Into<String>) -> Self {
        let issuer = issuer.into();
        let base = issuer.trim_end_matches('/').to_string();
        Self {
            client_id: client_id.into(),
            scopes: DEFAULT_SCOPES.iter().map(|s| (*s).to_string()).collect(),
            authorize_endpoint: format!("{base}/oauth/authorize"),
            token_endpoint: format!("{base}/oauth/token"),
            device_authorization_endpoint: format!("{base}/oauth/device/code"),
            issuer,
            vendor: fixed_vendor(DEFAULT_VENDOR),
            api_key_ref: fixed_ref(DEFAULT_API_KEY_REF),
            refresh_leeway: Duration::from_secs(60),
            login_timeout: Duration::from_secs(300),
            device_code_only: false,
            slow_down_increment: Duration::from_secs(5),
            callback_port: crate::ported::codex::authorize::DEFAULT_PORT,
            originator: DEFAULT_ORIGINATOR.to_string(),
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
}

impl Default for CodexAuthConfig {
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
