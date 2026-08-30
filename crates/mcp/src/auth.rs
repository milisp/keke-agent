//! Signing in to a remote MCP server.
//!
//! A server like `https://mcp.vercel.com` answers an unauthenticated request
//! with `401` and a `WWW-Authenticate` header naming where its metadata lives.
//! From there the MCP spec is ordinary OAuth 2.1 with three RFCs stacked on it:
//! protected-resource metadata (RFC 9728) says which authorization server
//! guards the resource, dynamic client registration (RFC 7591) obtains a
//! `client_id` — these servers hand out none in advance, so there is nothing to
//! configure — and resource indicators (RFC 8707) keep the issued token bound
//! to the server it was minted for.
//!
//! Two rules shape what is here:
//!
//! - **A browser never opens by itself.** Discovery happens inside a turn, and
//!   a turn that silently takes over the person's screen is a worse failure
//!   than one that says it needs a login. Signing in is something a person
//!   asks for — `keke mcp login`, or `/mcp` — and everything in this module is
//!   reached from one of those or from a token that already exists.
//! - **A registration is not a credential.** The `client_id` from dynamic
//!   registration is public by construction (these clients authenticate with
//!   `none`), so it lives in a plain file and survives `logout`; the tokens go
//!   to the same 0600 per-vendor store every provider login uses.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use keke_auth_api::AuthError;
use keke_auth_api::LoginUi;
use keke_credentials::AuthFile;
use keke_credentials::AuthMode;
use keke_credentials::AuthTokens;
use keke_credentials::Vendor;
use keke_credentials::VendorAuthStore;
use keke_oauth::Loopback;
use keke_oauth::Pkce;
use keke_oauth::random_token;
use keke_paths::AbsPath;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

/// The redirect path this client listens on. Any path works — the registration
/// is made naming this one, moments before it is used.
const CALLBACK_PATH: &str = "/callback";

/// A discovery or token request may not outlive the patience of whoever asked.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the person has to finish in the browser.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

/// Refresh this long before the token actually expires, so a request does not
/// race the clock it was checked against.
const REFRESH_MARGIN: i64 = 60;

/// Where MCP credentials and registrations are kept.
///
/// Composed by the host, which is the only layer that knows where the harness
/// keeps state. Remote MCP servers' tokens are filed under their own `mcp/`
/// subdirectory rather than beside every other provider's `auth.*.json`: a
/// project can configure many remote servers, and a flat directory shared with
/// provider logins turns unreadable long before a project's server count does.
#[derive(Clone, Debug)]
pub struct AuthHome {
    tokens: VendorAuthStore,
    clients: PathBuf,
}

impl AuthHome {
    #[must_use]
    pub fn new(home: &AbsPath) -> Self {
        let mcp = home.as_path().join("mcp");
        // Appending a fixed, non-empty relative segment to an already-absolute
        // path is always absolute; `AbsPath::new` cannot fail here.
        #[allow(clippy::expect_used)]
        let mcp_home = AbsPath::new(&mcp).expect("home joined with a relative segment is absolute");
        Self {
            tokens: VendorAuthStore::new(mcp_home),
            clients: mcp.join("clients.json"),
        }
    }
}

/// One remote server's credential.
#[derive(Debug)]
pub struct ServerAuth {
    home: AuthHome,
    /// The server as configured, which is both what is authenticated against
    /// and the key everything is filed under.
    url: String,
    /// The server's name, for messages a person has to act on.
    name: String,
    vendor: Vendor,
    http: reqwest::Client,
    /// The access token in force, avoiding a file read per request.
    cached: Mutex<Option<AuthTokens>>,
}

impl ServerAuth {
    /// Prepare to authenticate `name` at `url`.
    ///
    /// Nothing is read here: a server nobody has signed in to must cost no file
    /// access, because most sessions have no remote server at all.
    pub fn new(home: AuthHome, name: &str, url: &str) -> Result<Self, AuthError> {
        Ok(Self {
            vendor: Vendor::new(vendor_slug(name, url))
                .map_err(|err| AuthError::Other(err.to_string()))?,
            home,
            url: url.to_string(),
            name: name.to_string(),
            http: client()?,
            cached: Mutex::new(None),
        })
    }

    /// Whether a token is stored at all, whatever its state.
    pub fn has_credential(&self) -> bool {
        matches!(self.stored(), Ok(Some(_)))
    }

    /// A usable access token, refreshing first if the stored one has expired.
    ///
    /// `None` means "sign in", not "something went wrong": a server that needs
    /// no authentication and one that has never been signed in to both arrive
    /// here, and neither is an error to report from inside a turn.
    pub async fn bearer(&self) -> Option<String> {
        if let Some(token) = self.fresh_cached() {
            return Some(token);
        }
        let tokens = self.stored().ok().flatten()?;
        if !expired(&tokens) {
            let token = tokens.access_token.clone();
            self.cache(tokens);
            return Some(token);
        }
        self.refresh().await
    }

    /// Spend the refresh token. Returns the new access token, if there was one.
    ///
    /// Called both on expiry and on a 401 from a server that decided otherwise.
    pub async fn refresh(&self) -> Option<String> {
        let mutation = self.home.tokens.begin(&self.vendor).ok()?;
        // Re-read under the lock: another process may have refreshed while
        // this one waited, and spending a rotated refresh token fails.
        let file = mutation.load().ok().flatten()?;
        let tokens = file.tokens.clone()?;
        if !expired(&tokens) {
            let token = tokens.access_token.clone();
            self.cache(tokens);
            return Some(token);
        }
        let refresh_token = tokens.refresh_token.clone()?;
        let client = self.registration().ok().flatten()?;

        let issued = self
            .token_request(
                &client.token_endpoint,
                &[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", refresh_token.as_str()),
                    ("client_id", client.client_id.as_str()),
                    ("resource", self.url.as_str()),
                ],
            )
            .await
            .ok()?;

        // An issuer that rotates refresh tokens returns a new one; one that
        // does not returns nothing, and the old one stays valid.
        let stored = AuthTokens {
            refresh_token: issued.refresh_token.clone().or(Some(refresh_token)),
            issuer: tokens.issuer.clone(),
            ..issued.clone()
        };
        mutation
            .save(&AuthFile::from_tokens(AuthMode::Oidc, stored.clone()))
            .ok()?;
        let token = stored.access_token.clone();
        self.cache(stored);
        Some(token)
    }

    /// Run the whole flow: discover, register if needed, open a browser, and
    /// store what comes back.
    pub async fn login(&self, ui: &dyn LoginUi) -> Result<(), AuthError> {
        // Ask the server itself first. Its `401` names where its metadata is,
        // and that answer is authoritative — the well-known paths below are
        // the fallback for a server that challenges without saying.
        let hint = self.challenge().await;
        let protected = self.protected_resource(hint.as_deref()).await?;
        let issuer = protected
            .authorization_servers
            .first()
            .cloned()
            .ok_or_else(|| {
                AuthError::Other(format!(
                    "`{}` does not say which authorization server guards it",
                    self.name
                ))
            })?;
        let metadata = self.server_metadata(&issuer).await?;

        let loopback = Loopback::bind(0, CALLBACK_PATH)
            .await
            .map_err(|err| AuthError::Other(format!("no loopback port available: {err}")))?;
        let redirect_uri = loopback.redirect_uri()?;

        let client_id = self.client_id(&metadata, &redirect_uri).await?;
        let resource = protected
            .resource
            .clone()
            .unwrap_or_else(|| self.url.clone());

        let pkce = Pkce::generate();
        let state = random_token(16);
        let mut authorize = url::Url::parse(&metadata.authorization_endpoint)
            .map_err(|err| AuthError::Other(format!("authorize endpoint is not a URL: {err}")))?;
        {
            let mut query = authorize.query_pairs_mut();
            query
                .append_pair("response_type", "code")
                .append_pair("client_id", &client_id)
                .append_pair("redirect_uri", &redirect_uri)
                .append_pair("state", &state)
                .append_pair("code_challenge", &pkce.challenge)
                .append_pair("code_challenge_method", "S256")
                // Without this the issuer may mint a token for a different
                // audience, which a careful resource server then refuses.
                .append_pair("resource", &resource);
            if let Some(scope) = protected.scope() {
                query.append_pair("scope", &scope);
            }
        }

        ui.open_browser(authorize.as_str());
        ui.notice(&format!(
            "waiting for the browser to authorize `{}`",
            self.name
        ));

        let code = loopback.await_code(&state, LOGIN_TIMEOUT).await?;

        let issued = self
            .token_request(
                &metadata.token_endpoint,
                &[
                    ("grant_type", "authorization_code"),
                    ("code", code.as_str()),
                    ("redirect_uri", redirect_uri.as_str()),
                    ("client_id", client_id.as_str()),
                    ("code_verifier", pkce.verifier.as_str()),
                    ("resource", resource.as_str()),
                ],
            )
            .await?;

        let tokens = AuthTokens {
            issuer: Some(issuer.clone()),
            ..issued
        };
        self.home
            .tokens
            .begin(&self.vendor)
            .map_err(|err| AuthError::Other(err.to_string()))?
            .save(&AuthFile::from_tokens(AuthMode::Oidc, tokens.clone()))
            .map_err(|err| AuthError::Other(err.to_string()))?;
        self.cache(tokens);

        // The registration is kept: it is not a secret, and re-registering on
        // every login would leave a trail of dead clients at the issuer.
        self.remember(&Registration {
            client_id,
            authorization_endpoint: metadata.authorization_endpoint,
            token_endpoint: metadata.token_endpoint,
            registration_endpoint: metadata.registration_endpoint,
        })?;
        Ok(())
    }

    /// Discard the stored token, keeping the registration.
    pub fn logout(&self) -> Result<bool, AuthError> {
        if let Ok(mut cached) = self.cached.lock() {
            *cached = None;
        }
        self.home
            .tokens
            .delete(&self.vendor)
            .map_err(|err| AuthError::Other(err.to_string()))
    }

    // --- storage -----------------------------------------------------------

    fn stored(&self) -> Result<Option<AuthTokens>, AuthError> {
        Ok(self
            .home
            .tokens
            .load(&self.vendor)
            .map_err(|err| AuthError::Other(err.to_string()))?
            .and_then(|file| file.tokens))
    }

    fn fresh_cached(&self) -> Option<String> {
        let held = self.cached.lock().ok()?;
        let tokens = held.as_ref()?;
        (!expired(tokens)).then(|| tokens.access_token.clone())
    }

    fn cache(&self, tokens: AuthTokens) {
        if let Ok(mut held) = self.cached.lock() {
            *held = Some(tokens);
        }
    }

    /// The registration for this server, if one was made.
    fn registration(&self) -> Result<Option<Registration>, AuthError> {
        Ok(self.registrations()?.remove(&self.url))
    }

    fn registrations(&self) -> Result<BTreeMap<String, Registration>, AuthError> {
        match std::fs::read_to_string(&self.home.clients) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(err) => Err(AuthError::Other(format!(
                "reading {}: {err}",
                self.home.clients.display()
            ))),
            Ok(text) => serde_json::from_str(&text).map_err(|err| {
                AuthError::Other(format!(
                    "{} is not readable: {err}",
                    self.home.clients.display()
                ))
            }),
        }
    }

    fn remember(&self, registration: &Registration) -> Result<(), AuthError> {
        let mut all = self.registrations()?;
        all.insert(self.url.clone(), registration.clone());
        let text =
            serde_json::to_string_pretty(&all).map_err(|err| AuthError::Other(err.to_string()))?;
        std::fs::write(&self.home.clients, text + "\n").map_err(|err| {
            AuthError::Other(format!("writing {}: {err}", self.home.clients.display()))
        })
    }

    // --- discovery ---------------------------------------------------------

    /// Provoke the server's authentication challenge and read where it says
    /// its metadata lives.
    ///
    /// Any answer that is not a `401` means the server is not challenging us —
    /// it may need no authentication at all — and discovery falls through to
    /// the well-known paths rather than treating that as a failure.
    async fn challenge(&self) -> Option<String> {
        let response = self
            .http
            .post(&self.url)
            .header("accept", "application/json, text/event-stream")
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {},
            }))
            .send()
            .await
            .ok()?;
        if response.status() != reqwest::StatusCode::UNAUTHORIZED {
            return None;
        }
        resource_metadata_hint(
            response
                .headers()
                .get(reqwest::header::WWW_AUTHENTICATE)?
                .to_str()
                .ok()?,
        )
    }

    /// The server's protected-resource metadata (RFC 9728).
    ///
    /// `hint` is the URL a `WWW-Authenticate` header named, which is
    /// authoritative when present. Without one the well-known locations are
    /// tried in the order the RFC specifies: path-suffixed first, because a
    /// host serving several resources distinguishes them that way.
    async fn protected_resource(&self, hint: Option<&str>) -> Result<Protected, AuthError> {
        let mut candidates: Vec<String> = hint.into_iter().map(str::to_string).collect();
        candidates.extend(well_known(&self.url, "oauth-protected-resource"));

        for candidate in &candidates {
            if let Some(found) = self.fetch::<Protected>(candidate).await
                && !found.authorization_servers.is_empty()
            {
                return Ok(found);
            }
        }
        Err(AuthError::Other(format!(
            "`{}` published no OAuth metadata at {}, so keke cannot tell where to sign in",
            self.name,
            candidates.join(" or ")
        )))
    }

    /// The authorization server's metadata (RFC 8414), or the paths every
    /// issuer conventionally serves when it publishes none.
    async fn server_metadata(&self, issuer: &str) -> Result<ServerMetadata, AuthError> {
        let mut candidates = well_known(issuer, "oauth-authorization-server");
        candidates.extend(well_known(issuer, "openid-configuration"));

        for candidate in &candidates {
            if let Some(found) = self.fetch::<ServerMetadata>(candidate).await
                && !found.authorization_endpoint.is_empty()
                && !found.token_endpoint.is_empty()
            {
                return Ok(found);
            }
        }

        // A server that publishes nothing is not necessarily broken — plenty
        // serve the conventional paths — so the derived endpoints are tried
        // rather than the login being refused here.
        let base = issuer.trim_end_matches('/');
        Ok(ServerMetadata {
            authorization_endpoint: format!("{base}/authorize"),
            token_endpoint: format!("{base}/token"),
            registration_endpoint: Some(format!("{base}/register")),
            scopes_supported: Vec::new(),
        })
    }

    async fn fetch<T: serde::de::DeserializeOwned>(&self, url: &str) -> Option<T> {
        let response = self.http.get(url).send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        response.json::<T>().await.ok()
    }

    /// The `client_id` to authorize with: the one already registered, or a new
    /// registration made now (RFC 7591).
    async fn client_id(
        &self,
        metadata: &ServerMetadata,
        redirect_uri: &str,
    ) -> Result<String, AuthError> {
        // A registration is tied to its redirect URI, and the port changes per
        // login, so a stored one is reused only if this issuer accepts a
        // loopback URI it did not see — which is what `redirect_uris` below
        // registers. Re-registering per login would be correct too, and
        // noisier at the issuer.
        if let Some(existing) = self.registration()?
            && existing.token_endpoint == metadata.token_endpoint
        {
            return Ok(existing.client_id);
        }

        let endpoint = metadata.registration_endpoint.as_ref().ok_or_else(|| {
            AuthError::Other(format!(
                "`{}` offers no client registration and keke has no client id for it",
                self.name
            ))
        })?;

        let response = self
            .http
            .post(endpoint)
            .json(&serde_json::json!({
                "client_name": "keke",
                "redirect_uris": [redirect_uri],
                "grant_types": ["authorization_code", "refresh_token"],
                "response_types": ["code"],
                // A public client: there is no secret a terminal program could
                // keep, and saying so is what makes PKCE the proof instead.
                "token_endpoint_auth_method": "none",
            }))
            .send()
            .await
            .map_err(|err| {
                AuthError::Other(format!("could not register with `{}`: {err}", self.name))
            })?;

        let status = response.status();
        let body: Value = response
            .json()
            .await
            .map_err(|err| AuthError::Other(format!("registration answered unusably: {err}")))?;
        if !status.is_success() {
            return Err(AuthError::Rejected(format!(
                "`{}` refused to register a client ({status}): {}",
                self.name,
                describe(&body)
            )));
        }
        body.get("client_id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| AuthError::Other("the registration returned no client id".to_string()))
    }

    /// Post a form to the token endpoint and read the token set out of it.
    async fn token_request(
        &self,
        endpoint: &str,
        form: &[(&str, &str)],
    ) -> Result<AuthTokens, AuthError> {
        let response = self
            .http
            .post(endpoint)
            .form(form)
            .send()
            .await
            .map_err(|err| AuthError::Other(format!("the token endpoint is unreachable: {err}")))?;

        let status = response.status();
        let body: Value = response.json().await.map_err(|err| {
            AuthError::Other(format!("the token endpoint answered unusably: {err}"))
        })?;
        if !status.is_success() {
            return Err(AuthError::Rejected(format!(
                "the token endpoint refused ({status}): {}",
                describe(&body)
            )));
        }

        let access_token = body
            .get("access_token")
            .and_then(Value::as_str)
            .ok_or_else(|| AuthError::Other("the token response carried no access token".into()))?;

        Ok(AuthTokens {
            access_token: access_token.to_string(),
            refresh_token: body
                .get("refresh_token")
                .and_then(Value::as_str)
                .map(str::to_string),
            expires_at: body
                .get("expires_in")
                .and_then(Value::as_i64)
                .map(|seconds| now() + seconds),
            ..AuthTokens::default()
        })
    }
}

/// A dynamic client registration, and where it is good for.
///
/// Stored with its endpoints so a refresh needs no discovery round trip — and
/// so a registration is discarded when the issuer moves, rather than being sent
/// somewhere it means nothing.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct Registration {
    client_id: String,
    authorization_endpoint: String,
    token_endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    registration_endpoint: Option<String>,
}

/// Protected-resource metadata: which authorization server guards this, and
/// what to ask it for.
#[derive(Clone, Debug, Default, Deserialize)]
struct Protected {
    #[serde(default)]
    authorization_servers: Vec<String>,
    /// The canonical identifier of the resource, which is what the token is
    /// bound to. Absent means the server URL itself.
    #[serde(default)]
    resource: Option<String>,
    #[serde(default)]
    scopes_supported: Vec<String>,
}

impl Protected {
    fn scope(&self) -> Option<String> {
        (!self.scopes_supported.is_empty()).then(|| self.scopes_supported.join(" "))
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
struct ServerMetadata {
    #[serde(default)]
    authorization_endpoint: String,
    #[serde(default)]
    token_endpoint: String,
    #[serde(default)]
    registration_endpoint: Option<String>,
    #[serde(default)]
    #[allow(
        dead_code,
        reason = "read from metadata for completeness; scopes come from the resource"
    )]
    scopes_supported: Vec<String>,
}

/// The `resource_metadata` a `401` pointed at, if it named one.
///
/// The header is `Bearer realm="…", resource_metadata="https://…"`. Only that
/// one parameter is read: everything else in an auth challenge describes how to
/// present a token keke does not have yet.
fn resource_metadata_hint(header: &str) -> Option<String> {
    let start = header.find("resource_metadata=")? + "resource_metadata=".len();
    let rest = &header[start..];
    let value = match rest.strip_prefix('"') {
        Some(quoted) => quoted.split('"').next()?,
        None => rest.split(',').next()?.trim(),
    };
    (!value.is_empty()).then(|| value.to_string())
}

/// The well-known locations for `document` about `url`, RFC 8414 §3.1 order.
fn well_known(url: &str, document: &str) -> Vec<String> {
    let Ok(parsed) = url::Url::parse(url) else {
        return Vec::new();
    };
    let origin = match parsed.host_str() {
        Some(host) => {
            let scheme = parsed.scheme();
            match parsed.port() {
                Some(port) => format!("{scheme}://{host}:{port}"),
                None => format!("{scheme}://{host}"),
            }
        }
        None => return Vec::new(),
    };
    let path = parsed.path().trim_end_matches('/');

    let mut candidates = Vec::new();
    if !path.is_empty() {
        candidates.push(format!("{origin}/.well-known/{document}{path}"));
    }
    candidates.push(format!("{origin}/.well-known/{document}"));
    candidates
}

/// The file name a server's tokens are filed under, within [`AuthHome`]'s
/// `mcp/` directory.
///
/// Derived from the name *and* the URL: two projects may each configure a
/// `github` server pointing somewhere different, and a shared file would hand
/// one of them the other's token.
fn vendor_slug(name: &str, url: &str) -> String {
    let mut slug = String::new();
    for ch in name.chars() {
        match ch {
            'a'..='z' | '0'..='9' => slug.push(ch),
            'A'..='Z' => slug.push(ch.to_ascii_lowercase()),
            _ => slug.push('-'),
        }
    }
    // `Vendor` requires the slug to start with a lowercase letter or digit,
    // which a server name is not guaranteed to: a name starting with a symbol
    // like `!` would otherwise be rejected.
    if !slug.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit()) {
        slug.insert(0, 's');
    }
    // A short digest of the URL rather than the URL itself: the slug is a file
    // name, and a person reading their own directory should not find a full
    // endpoint spelled out in it.
    slug.push('-');
    slug.push_str(&digest(url));
    slug
}

/// Eight hex characters of FNV-1a. Not a security primitive — this only has to
/// keep two servers' files apart.
fn digest(value: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:08x}", hash as u32)
}

fn expired(tokens: &AuthTokens) -> bool {
    tokens
        .expires_at
        .is_some_and(|at| at - REFRESH_MARGIN <= now())
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or_default()
}

/// An OAuth error body, as something worth printing.
fn describe(body: &Value) -> String {
    let field = |name: &str| body.get(name).and_then(Value::as_str);
    match (field("error"), field("error_description")) {
        (_, Some(description)) => description.to_string(),
        (Some(error), None) => error.to_string(),
        (None, None) => "no reason given".to_string(),
    }
}

fn client() -> Result<reqwest::Client, AuthError> {
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|err| AuthError::Other(format!("could not build an HTTP client: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_challenge_names_where_the_metadata_is() {
        assert_eq!(
            resource_metadata_hint(
                r#"Bearer realm="x", resource_metadata="https://mcp.test/.well-known/oauth-protected-resource""#
            )
            .as_deref(),
            Some("https://mcp.test/.well-known/oauth-protected-resource")
        );
        assert_eq!(
            resource_metadata_hint("Bearer resource_metadata=https://mcp.test/m, realm=x")
                .as_deref(),
            Some("https://mcp.test/m")
        );
        // A plain challenge names nothing, and the well-known paths are tried.
        assert_eq!(resource_metadata_hint("Bearer realm=\"x\""), None);
    }

    #[test]
    fn the_path_suffixed_well_known_is_tried_first() {
        assert_eq!(
            well_known("https://mcp.vercel.com/mcp", "oauth-protected-resource"),
            vec![
                "https://mcp.vercel.com/.well-known/oauth-protected-resource/mcp",
                "https://mcp.vercel.com/.well-known/oauth-protected-resource",
            ]
        );
        // A bare origin has no suffix to try.
        assert_eq!(
            well_known("https://mcp.vercel.com", "oauth-protected-resource"),
            vec!["https://mcp.vercel.com/.well-known/oauth-protected-resource"]
        );
    }

    #[test]
    fn two_servers_of_the_same_name_do_not_share_a_credential_file() {
        let one = vendor_slug("github", "https://a.test/mcp");
        let two = vendor_slug("github", "https://b.test/mcp");
        assert_ne!(one, two);
        assert!(one.starts_with("github-"), "{one}");
        // It is a file name, so it has to survive `Vendor`'s validation.
        assert!(Vendor::new(one).is_ok());
        assert!(Vendor::new(vendor_slug("Weird Name!", "https://c.test")).is_ok());
    }

    #[test]
    fn a_token_is_refreshed_before_it_actually_expires() {
        let almost = AuthTokens {
            access_token: "a".into(),
            expires_at: Some(now() + 5),
            ..AuthTokens::default()
        };
        assert!(expired(&almost), "a token about to expire is not usable");

        let fine = AuthTokens {
            access_token: "a".into(),
            expires_at: Some(now() + 600),
            ..AuthTokens::default()
        };
        assert!(!expired(&fine));

        // An issuer that names no expiry is taken at its word.
        assert!(!expired(&AuthTokens::bearer("a")));
    }
}
