//! xAI authentication: OAuth2 over a loopback redirect, RFC 8628 device code,
//! and a long-lived API key.
//!
//! Nothing here caches a resolved credential across operations. `headers` reads
//! the store on every request so a refresh — which may have happened in another
//! task, in another process — reaches the next call without a restart.
//!
//! Two stores, for two kinds of credential. Anything a login minted lives in
//! `auth.grok.json` through a [`VendorAuthStore`]; an `XAI_API_KEY` the
//! deployment supplied resolves through the layered [`CredentialStore`], which
//! is what keeps `KEKE_CREDENTIAL_STORE=file` and the environment layer
//! meaningful.

mod config;
mod device;
mod endpoint;
mod jwt;
mod loopback;
mod pkce;
mod tokens;

#[cfg(test)]
mod test_support;

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use keke_auth_api::AuthError;
use keke_auth_api::AuthFuture;
use keke_auth_api::AuthHeaders;
use keke_auth_api::AuthProvider;
use keke_auth_api::CredentialSnapshot;
use keke_auth_api::CredentialStore;
use keke_auth_api::LoginUi;
use keke_credentials::AuthFile;
use keke_credentials::AuthMode;
use keke_credentials::AuthTokens;
use keke_credentials::Importer;
use keke_credentials::VendorAuthStore;
use sha2::Digest as _;
use sha2::Sha256;
use tokio::sync::Mutex;

pub use config::DEFAULT_API_KEY_REF;
pub use config::DEFAULT_CLIENT_ID;
pub use config::DEFAULT_ISSUER;
pub use config::DEFAULT_VENDOR;
pub use config::GrokAuthConfig;

use crate::device::Delay;
use crate::device::TokioDelay;
use crate::tokens::SOURCE_ENV;

pub const AUTH_ID: &str = "grok";

/// Outcome of the last refresh, and the generation it belongs to.
///
/// The generation is what makes a refresh single-flight rather than merely
/// serialised: a caller that queued behind the refresh compares the generation
/// it saw on arrival with the one it finds on entry, and a bump means somebody
/// else already did the work it was waiting to do.
struct Refresh {
    generation: u64,
    succeeded: bool,
}

/// The xAI [`AuthProvider`].
pub struct GrokAuth {
    config: GrokAuthConfig,
    credentials: Arc<dyn CredentialStore>,
    auth_files: VendorAuthStore,
    importer: Importer,
    http: reqwest::Client,
    refresh: Mutex<Refresh>,
    generation: AtomicU64,
    delay: Arc<dyn Delay>,
}

impl GrokAuth {
    pub fn new(
        credentials: Arc<dyn CredentialStore>,
        auth_files: VendorAuthStore,
        config: GrokAuthConfig,
    ) -> Self {
        Self {
            config,
            credentials,
            auth_files,
            importer: Importer::from_env(),
            http: reqwest::Client::new(),
            refresh: Mutex::new(Refresh {
                generation: 0,
                succeeded: false,
            }),
            generation: AtomicU64::new(0),
            delay: Arc::new(TokioDelay),
        }
    }

    /// Against the public xAI issuer.
    pub fn with_defaults(
        credentials: Arc<dyn CredentialStore>,
        auth_files: VendorAuthStore,
    ) -> Self {
        Self::new(credentials, auth_files, GrokAuthConfig::default())
    }

    /// Point the grok-CLI import at a different home.
    ///
    /// A test must never read the developer's real `~/.grok`, and redirecting
    /// it by mutating `$GROK_HOME` is both `unsafe` in this edition and shared
    /// between parallel tests.
    #[must_use]
    pub fn with_importer(mut self, importer: Importer) -> Self {
        self.importer = importer;
        self
    }

    #[must_use]
    pub fn config(&self) -> &GrokAuthConfig {
        &self.config
    }

    #[cfg(test)]
    fn with_delay(mut self, delay: Arc<dyn Delay>) -> Self {
        self.delay = delay;
        self
    }

    /// The credential in force, applying the precedence rule: an explicit
    /// `keke login` result — the auth file — wins over an imported grok CLI
    /// login, which wins over nothing.
    fn credential(&self) -> Result<Option<AuthFile>, AuthError> {
        if let Some(file) = self
            .auth_files
            .load(&self.config.vendor)?
            .filter(AuthFile::has_credential)
        {
            return Ok(Some(file));
        }
        Ok(self.imported())
    }

    /// A grok CLI login, if there is one to adopt.
    ///
    /// An unreadable foreign file is a warning rather than an error: it is not
    /// keke's file, and refusing to authenticate because another tool left a
    /// world-readable one would be keke's problem to answer for.
    fn imported(&self) -> Option<AuthFile> {
        match self.importer.import(&self.config.vendor) {
            Ok(found) => found.map(|found| found.auth),
            Err(err) => {
                tracing::warn!(auth = AUTH_ID, %err, "ignoring an existing grok CLI login");
                None
            }
        }
    }

    fn tokens(&self) -> Result<Option<AuthTokens>, AuthError> {
        Ok(self.credential()?.and_then(|file| file.tokens))
    }

    fn save(&self, mode: AuthMode, tokens: AuthTokens) -> Result<(), AuthError> {
        self.auth_files
            .save(&self.config.vendor, &AuthFile::from_tokens(mode, tokens))?;
        Ok(())
    }

    /// An API key the deployment supplied, from the layered credential store.
    fn api_key(&self) -> Result<Option<String>, AuthError> {
        Ok(self.credentials.load(&self.config.api_key_ref)?)
    }

    /// Refresh at most once, however many callers ask at once.
    async fn refresh_once(&self) -> bool {
        let observed = self.generation.load(Ordering::Acquire);
        let mut state = self.refresh.lock().await;
        if state.generation != observed {
            return state.succeeded;
        }

        let succeeded = match self.perform_refresh().await {
            Ok(()) => true,
            Err(err) => {
                tracing::warn!(auth = AUTH_ID, error = %err, "xai token refresh failed");
                false
            }
        };

        state.generation = state.generation.wrapping_add(1);
        state.succeeded = succeeded;
        self.generation.store(state.generation, Ordering::Release);
        succeeded
    }

    async fn perform_refresh(&self) -> Result<(), AuthError> {
        let current = self
            .credential()?
            .ok_or_else(|| AuthError::NotConfigured(AUTH_ID.to_string()))?;
        let tokens = current
            .tokens
            .ok_or_else(|| AuthError::RefreshFailed("the credential is not refreshable".into()))?;
        let refresh_token = tokens.refresh_token.clone().ok_or_else(|| {
            AuthError::RefreshFailed("the stored credential has no refresh token".into())
        })?;

        let response = endpoint::exchange(
            &self.http,
            &self.config.token_endpoint,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token.as_str()),
                ("client_id", self.config.client_id.as_str()),
            ],
        )
        .await?;

        self.save(
            current.auth_mode,
            response.into_tokens(Some(refresh_token), tokens.account_id),
        )
    }
}

/// A stable, non-secret name for a credential that has no readable identity.
///
/// A UUIDv5-shaped digest under a namespace: enough to correlate log lines
/// about the same credential, useless for reconstructing it.
fn stable_id(namespace: &str, secret: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(namespace.as_bytes());
    hasher.update([0u8]);
    hasher.update(secret.as_bytes());
    let digest = hasher.finalize();

    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;

    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

impl AuthProvider for GrokAuth {
    fn id(&self) -> &'static str {
        AUTH_ID
    }

    fn snapshot(&self) -> CredentialSnapshot {
        let base = CredentialSnapshot {
            auth_id: AUTH_ID.to_string(),
            ..CredentialSnapshot::default()
        };

        if let Some(file) = self.credential().ok().flatten() {
            if let Some(tokens) = file.tokens {
                let claims = jwt::claims(&tokens.access_token).unwrap_or_default();
                return CredentialSnapshot {
                    source: file.auth_mode.as_str().to_string(),
                    account_id: claims
                        .sub
                        .or(tokens.account_id.clone())
                        .or_else(|| Some(stable_id("xai-access-token", &tokens.access_token))),
                    organization_id: claims.org_id,
                    expires_at: tokens::expires_at(&tokens),
                    ..base
                };
            }
            if let Some(key) = file.api_key {
                return CredentialSnapshot {
                    source: file.auth_mode.as_str().to_string(),
                    account_id: Some(stable_id("xai-api-key", &key)),
                    ..base
                };
            }
        }

        match self.api_key().ok().flatten() {
            Some(key) => CredentialSnapshot {
                source: SOURCE_ENV.to_string(),
                account_id: Some(stable_id("xai-api-key", &key)),
                ..base
            },
            None => base,
        }
    }

    fn has_usable_credential(&self) -> bool {
        self.credential().ok().flatten().is_some() || self.api_key().ok().flatten().is_some()
    }

    fn headers(&self) -> AuthFuture<'_, Result<AuthHeaders, AuthError>> {
        Box::pin(async move {
            if let Some(file) = self.credential()? {
                if let Some(tokens) = file.tokens {
                    if !tokens::is_stale(&tokens, self.config.refresh_leeway) {
                        return Ok(AuthHeaders::bearer(&tokens.access_token));
                    }
                    if !self.refresh_once().await {
                        return Err(AuthError::RefreshFailed(
                            "the xAI access token expired and could not be renewed".into(),
                        ));
                    }
                    let refreshed = self.tokens()?.ok_or_else(|| {
                        AuthError::RefreshFailed("the refreshed credential vanished".into())
                    })?;
                    return Ok(AuthHeaders::bearer(&refreshed.access_token));
                }
                if let Some(key) = file.api_key {
                    return Ok(AuthHeaders::bearer(&key));
                }
            }

            match self.api_key()? {
                Some(key) => Ok(AuthHeaders::bearer(&key)),
                None => Err(AuthError::NotConfigured(AUTH_ID.to_string())),
            }
        })
    }

    /// An existing grok CLI login is adopted in place of a browser flow, but
    /// only when keke holds no credential of its own — see
    /// [`keke_credentials::Importer`] for why that ordering is the whole point.
    fn login<'a>(&'a self, ui: Arc<dyn LoginUi>) -> AuthFuture<'a, Result<(), AuthError>> {
        Box::pin(async move {
            let stored = self.auth_files.load(&self.config.vendor)?;
            if !stored.is_some_and(|file| file.has_credential())
                && let Some(imported) = self.imported()
            {
                ui.notice("adopting the existing grok CLI login");
                self.auth_files.save(&self.config.vendor, &imported)?;
                return Ok(());
            }

            let listener = if self.config.device_code_only {
                None
            } else {
                match loopback::bind().await {
                    Ok(listener) => Some(listener),
                    Err(err) => {
                        tracing::warn!(
                            auth = AUTH_ID,
                            %err,
                            "no loopback port available; falling back to the device code flow"
                        );
                        None
                    }
                }
            };

            // The mode records how the credential was obtained, which is what
            // a person reading `auth.grok.json` next to the grok CLI's own
            // `auth.json` needs in order to recognize it.
            let (mode, tokens) = match listener {
                Some(listener) => (
                    AuthMode::Oidc,
                    loopback::run(&self.http, &self.config, ui.as_ref(), listener).await?,
                ),
                None => (
                    AuthMode::DeviceCode,
                    device::run(&self.http, &self.config, ui.as_ref(), self.delay.as_ref()).await?,
                ),
            };

            self.save(mode, tokens)
        })
    }

    fn refresh_after_unauthorized(&self) -> AuthFuture<'_, bool> {
        Box::pin(self.refresh_once())
    }

    /// Clears only the credential this plugin minted. An API key the deployment
    /// supplied is not ours to revoke, and deleting it would make `logout` a
    /// way to lose a secret keke never issued.
    fn logout(&self) -> AuthFuture<'_, Result<(), AuthError>> {
        Box::pin(async move {
            self.auth_files.delete(&self.config.vendor)?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Home;
    use crate::test_support::RecordingUi;
    use crate::test_support::store_tokens;
    use crate::test_support::stored_tokens;
    use crate::test_support::xai;
    use keke_credentials::MemoryStore;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    fn grok_cli_login() -> serde_json::Value {
        serde_json::json!({
            "https://auth.x.ai": {
                "key": "from-grok-cli",
                "auth_mode": "oidc",
                "create_time": "2026-08-01T00:00:00Z",
                "user_id": "user-3",
                "refresh_token": "grok-cli-refresh",
            },
        })
    }

    #[test]
    fn the_registry_key_is_stable() {
        let home = Home::new();
        let auth = xai(
            &home,
            &Arc::new(MemoryStore::new()),
            GrokAuthConfig::default(),
        );
        assert_eq!(auth.id(), "grok");
    }

    #[test]
    fn the_credential_lands_in_a_file_named_after_the_vendor() {
        let home = Home::new();
        let auth = xai(
            &home,
            &Arc::new(MemoryStore::new()),
            GrokAuthConfig::default(),
        );
        let path = auth
            .auth_files
            .path(&auth.config().vendor)
            .expect("path")
            .to_string();
        assert!(path.ends_with("auth.grok.json"), "{path}");
    }

    #[test]
    fn a_stable_id_is_derived_and_not_the_secret() {
        let id = stable_id("xai-api-key", "xai-super-secret");
        assert_eq!(id.len(), 36);
        assert!(!id.contains("secret"));
        assert_eq!(id, stable_id("xai-api-key", "xai-super-secret"));
        assert_ne!(id, stable_id("xai-access-token", "xai-super-secret"));
    }

    #[tokio::test]
    async fn a_blank_stored_credential_is_not_a_credential() {
        let home = Home::new();
        let store = Arc::new(MemoryStore::new());
        let auth = xai(&home, &store, GrokAuthConfig::default());
        store_tokens(&auth, "   ".into(), None, AuthMode::Oidc);
        store.save(&auth.config.api_key_ref, "   ").expect("save");

        assert!(!auth.has_usable_credential());
        assert!(matches!(
            auth.headers().await,
            Err(AuthError::NotConfigured(_))
        ));
    }

    #[tokio::test]
    async fn a_snapshot_never_carries_the_token() {
        let home = Home::new();
        let store = Arc::new(MemoryStore::new());
        let auth = xai(&home, &store, GrokAuthConfig::default());
        store_tokens(
            &auth,
            jwt::encode_unsigned(r#"{"exp":4102444800,"sub":"user-7","org_id":"org-3"}"#),
            Some("refresh"),
            AuthMode::DeviceCode,
        );

        let snapshot = auth.snapshot();
        assert_eq!(snapshot.auth_id, "grok");
        assert_eq!(snapshot.source, "device-code");
        assert_eq!(snapshot.account_id.as_deref(), Some("user-7"));
        assert_eq!(snapshot.organization_id.as_deref(), Some("org-3"));
        assert_eq!(snapshot.expires_at, Some(4102444800));
    }

    #[tokio::test]
    async fn an_api_key_is_the_fallback_and_reports_its_source() {
        let home = Home::new();
        let store = Arc::new(MemoryStore::new());
        let auth = xai(&home, &store, GrokAuthConfig::default());
        store
            .save(&auth.config.api_key_ref, "xai-key-1")
            .expect("save");

        let headers: Vec<_> = auth
            .headers()
            .await
            .expect("headers")
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        assert_eq!(
            headers,
            vec![("authorization".to_string(), "Bearer xai-key-1".to_string())]
        );
        assert_eq!(auth.snapshot().source, "env");
        assert!(auth.has_usable_credential());
    }

    #[tokio::test]
    async fn logout_clears_the_minted_credential_but_not_a_supplied_key() {
        let home = Home::new();
        let store = Arc::new(MemoryStore::new());
        let auth = xai(&home, &store, GrokAuthConfig::default());
        store_tokens(&auth, "access".into(), None, AuthMode::Oidc);
        store
            .save(&auth.config.api_key_ref, "xai-key-1")
            .expect("save");

        auth.logout().await.expect("logout");
        assert!(stored_tokens(&auth).is_none());
        assert_eq!(
            store
                .load(&auth.config.api_key_ref)
                .expect("load")
                .as_deref(),
            Some("xai-key-1")
        );
    }

    #[tokio::test]
    async fn an_existing_grok_cli_login_is_used_instead_of_a_browser_flow() {
        let home = Home::new();
        let auth = GrokAuth::new(
            Arc::new(MemoryStore::new()),
            home.auth_files(),
            GrokAuthConfig::default(),
        )
        .with_importer(home.with_grok_cli_login(grok_cli_login()));

        let ui = RecordingUi::new();
        auth.login(ui.clone()).await.expect("login");

        assert!(
            ui.browser_urls().is_empty(),
            "an adoptable login must not open a browser"
        );
        assert_eq!(
            stored_tokens(&auth).expect("tokens").access_token,
            "from-grok-cli"
        );
    }

    #[tokio::test]
    async fn an_explicit_login_result_wins_over_an_available_import() {
        let home = Home::new();
        let auth = GrokAuth::new(
            Arc::new(MemoryStore::new()),
            home.auth_files(),
            GrokAuthConfig::default(),
        )
        .with_importer(home.with_grok_cli_login(grok_cli_login()));
        store_tokens(&auth, "from-keke-login".into(), None, AuthMode::Oidc);

        let headers = auth.headers().await.expect("headers");
        assert_eq!(
            headers.iter().collect::<Vec<_>>(),
            vec![("authorization", "Bearer from-keke-login")],
            "the auth file keke wrote must outrank another tool's login"
        );
    }

    #[tokio::test]
    async fn concurrent_unauthorized_replies_produce_exactly_one_refresh() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth2/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "access-2",
                "refresh_token": "refresh-2",
                "expires_in": 3600,
            })))
            .mount(&server)
            .await;

        let home = Home::new();
        let store = Arc::new(MemoryStore::new());
        let auth = xai(&home, &store, GrokAuthConfig::new(server.uri(), "client-1"));
        store_tokens(&auth, "access-1".into(), Some("refresh-1"), AuthMode::Oidc);

        let outcomes = tokio::join!(
            auth.refresh_after_unauthorized(),
            auth.refresh_after_unauthorized(),
            auth.refresh_after_unauthorized(),
            auth.refresh_after_unauthorized(),
        );
        assert_eq!(outcomes, (true, true, true, true));
        assert_eq!(
            server.received_requests().await.expect("requests").len(),
            1,
            "four concurrent 401s must renew the credential once"
        );
        assert_eq!(
            stored_tokens(&auth).expect("tokens").access_token,
            "access-2"
        );
    }

    #[tokio::test]
    async fn a_later_unauthorized_reply_refreshes_again() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth2/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "access-2",
            })))
            .mount(&server)
            .await;

        let home = Home::new();
        let store = Arc::new(MemoryStore::new());
        let auth = xai(&home, &store, GrokAuthConfig::new(server.uri(), "client-1"));
        store_tokens(&auth, "access-1".into(), Some("refresh-1"), AuthMode::Oidc);

        assert!(auth.refresh_after_unauthorized().await);
        assert!(auth.refresh_after_unauthorized().await);
        assert_eq!(server.received_requests().await.expect("requests").len(), 2);
    }

    #[tokio::test]
    async fn an_expired_access_token_is_refreshed_before_headers_are_produced() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth2/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "access-2",
                "expires_in": 3600,
            })))
            .mount(&server)
            .await;

        let home = Home::new();
        let store = Arc::new(MemoryStore::new());
        let auth = xai(&home, &store, GrokAuthConfig::new(server.uri(), "client-1"));
        let expired = jwt::encode_unsigned(&format!(r#"{{"exp":{}}}"#, tokens::now() - 30));
        store_tokens(&auth, expired, Some("refresh-1"), AuthMode::Oidc);

        let headers = auth.headers().await.expect("headers");
        assert_eq!(
            headers.iter().collect::<Vec<_>>(),
            vec![("authorization", "Bearer access-2")]
        );
        assert_eq!(server.received_requests().await.expect("requests").len(), 1);
    }

    #[tokio::test]
    async fn a_token_that_is_still_good_is_used_as_is() {
        let server = MockServer::start().await;
        let home = Home::new();
        let store = Arc::new(MemoryStore::new());
        let auth = xai(&home, &store, GrokAuthConfig::new(server.uri(), "client-1"));
        let fresh = jwt::encode_unsigned(&format!(r#"{{"exp":{}}}"#, tokens::now() + 3600));
        store_tokens(&auth, fresh.clone(), Some("refresh-1"), AuthMode::Oidc);

        let headers = auth.headers().await.expect("headers");
        assert_eq!(
            headers.iter().collect::<Vec<_>>(),
            vec![("authorization", format!("Bearer {fresh}").as_str())]
        );
        assert!(
            server
                .received_requests()
                .await
                .expect("requests")
                .is_empty(),
            "a live token must not cost a round trip"
        );
    }

    #[tokio::test]
    async fn a_refused_refresh_is_reported_rather_than_retried_forever() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth2/token"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_json(serde_json::json!({ "error": "invalid_grant" })),
            )
            .mount(&server)
            .await;

        let home = Home::new();
        let store = Arc::new(MemoryStore::new());
        let auth = xai(&home, &store, GrokAuthConfig::new(server.uri(), "client-1"));
        let expired = jwt::encode_unsigned(&format!(r#"{{"exp":{}}}"#, tokens::now() - 30));
        store_tokens(&auth, expired, Some("refresh-1"), AuthMode::Oidc);

        assert!(matches!(
            auth.headers().await,
            Err(AuthError::RefreshFailed(_))
        ));
    }
}
