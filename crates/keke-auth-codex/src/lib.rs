//! ChatGPT authentication: OAuth2 over a loopback redirect, RFC 8628 device
//! code, an existing codex CLI login, and a long-lived API key.
//!
//! Nothing here caches a resolved credential across operations. `headers` reads
//! the store on every request so a refresh — which may have happened in another
//! task, in another process — reaches the next call without a restart.
//!
//! Two stores, for two kinds of credential. Anything a login minted lives in
//! `auth.codex.json` through a [`VendorAuthStore`]; an `OPENAI_API_KEY` the
//! deployment supplied resolves through the layered [`CredentialStore`], which
//! is what keeps `KEKE_CREDENTIAL_STORE=file` and the environment layer
//! meaningful.

mod config;
mod device;
mod endpoint;
mod jwt;
mod loopback;
mod pkce;
mod ported;
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

pub use config::CodexAuthConfig;
pub use config::DEFAULT_API_KEY_REF;
pub use config::DEFAULT_CLIENT_ID;
pub use config::DEFAULT_ISSUER;
pub use config::DEFAULT_ORIGINATOR;
pub use config::DEFAULT_VENDOR;

use crate::device::Delay;
use crate::device::TokioDelay;
use crate::tokens::SOURCE_ENV;

pub const AUTH_ID: &str = "codex";

/// Outcome of the last refresh, and the generation it belongs to.
///
/// The generation is what makes a refresh single-flight rather than merely
/// serialised: a caller that queued behind the refresh compares the generation
/// it saw on arrival with the one it finds on entry, and a bump means somebody
/// else already did the work it was waiting to do.
struct Refresh {
    generation: u64,
    /// The failure text of the last attempt, so a caller that queued behind it
    /// is told why rather than merely that it did not work.
    outcome: Result<(), String>,
}

/// Why a refresh was asked for.
///
/// A 401 is not a clock: the issuer rejected a token this process still
/// believes is live, so the stored credential being fresh proves nothing and
/// the exchange happens anyway. An expiry check, by contrast, is satisfied by
/// whatever another process already wrote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RefreshMode {
    IfStale,
    Force,
}

/// The ChatGPT [`AuthProvider`].
pub struct CodexAuth {
    config: CodexAuthConfig,
    credentials: Arc<dyn CredentialStore>,
    auth_files: VendorAuthStore,
    importer: Importer,
    http: reqwest::Client,
    refresh: Mutex<Refresh>,
    generation: AtomicU64,
    delay: Arc<dyn Delay>,
}

impl CodexAuth {
    pub fn new(
        credentials: Arc<dyn CredentialStore>,
        auth_files: VendorAuthStore,
        config: CodexAuthConfig,
    ) -> Self {
        Self {
            config,
            credentials,
            auth_files,
            importer: Importer::from_env(),
            http: reqwest::Client::new(),
            refresh: Mutex::new(Refresh {
                generation: 0,
                outcome: Ok(()),
            }),
            generation: AtomicU64::new(0),
            delay: Arc::new(TokioDelay),
        }
    }

    /// Against the public OpenAI issuer.
    pub fn with_defaults(
        credentials: Arc<dyn CredentialStore>,
        auth_files: VendorAuthStore,
    ) -> Self {
        Self::new(credentials, auth_files, CodexAuthConfig::default())
    }

    /// Point the codex-CLI import at a different home.
    ///
    /// A test must never read the developer's real `~/.codex`, and redirecting
    /// it by mutating `$CODEX_HOME` is both `unsafe` in this edition and shared
    /// between parallel tests.
    #[must_use]
    pub fn with_importer(mut self, importer: Importer) -> Self {
        self.importer = importer;
        self
    }

    #[must_use]
    pub fn config(&self) -> &CodexAuthConfig {
        &self.config
    }

    #[cfg(test)]
    fn with_delay(mut self, delay: Arc<dyn Delay>) -> Self {
        self.delay = delay;
        self
    }

    /// The credential in force, applying the precedence rule: an explicit
    /// `keke login` result — the auth file — wins over an imported codex CLI
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

    /// A codex CLI login, if there is one to adopt.
    ///
    /// An unreadable foreign file is a warning rather than an error: it is not
    /// keke's file, and refusing to authenticate because another tool left a
    /// world-readable one would be keke's problem to answer for.
    fn imported(&self) -> Option<AuthFile> {
        match self.importer.import(&self.config.vendor) {
            Ok(found) => found.map(|found| found.auth),
            Err(err) => {
                tracing::warn!(auth = AUTH_ID, %err, "ignoring an existing codex CLI login");
                None
            }
        }
    }

    fn tokens(&self) -> Result<Option<AuthTokens>, AuthError> {
        Ok(self.credential()?.and_then(|file| file.tokens))
    }

    /// Every login this plugin performs is a ChatGPT login, so the file it
    /// writes is the one codex itself would recognize.
    fn save(&self, tokens: AuthTokens) -> Result<(), AuthError> {
        self.auth_files.save(
            &self.config.vendor,
            &AuthFile::from_tokens(AuthMode::Chatgpt, tokens),
        )?;
        Ok(())
    }

    /// An API key the deployment supplied, from the layered credential store.
    fn api_key(&self) -> Result<Option<String>, AuthError> {
        Ok(self.credentials.load(&self.config.api_key_ref)?)
    }

    /// Refresh at most once, however many callers ask at once.
    ///
    /// The outcome is the failure itself rather than a bool: "could not be
    /// renewed" with the reason discarded is indistinguishable between a
    /// blocked network and a refresh token the issuer has revoked, and those
    /// call for opposite responses from the person reading it.
    async fn refresh_once(&self, mode: RefreshMode) -> Result<(), AuthError> {
        let observed = self.generation.load(Ordering::Acquire);
        let mut state = self.refresh.lock().await;
        if state.generation != observed {
            return state.outcome.clone().map_err(AuthError::RefreshFailed);
        }

        let outcome = match self.perform_refresh(mode).await {
            Ok(()) => Ok(()),
            Err(err) => {
                tracing::warn!(auth = AUTH_ID, error = %err, "chatgpt token refresh failed");
                Err(err.to_string())
            }
        };

        state.generation = state.generation.wrapping_add(1);
        state.outcome = outcome.clone();
        self.generation.store(state.generation, Ordering::Release);
        outcome.map_err(AuthError::RefreshFailed)
    }

    async fn perform_refresh(&self, mode: RefreshMode) -> Result<(), AuthError> {
        // The lock is taken before the credential is read and held past the
        // write: between an unlocked read and the exchange, another keke
        // process can rotate the refresh token, and presenting the superseded
        // one gets `invalid_grant` — which reads as a revoked login.
        let mutation = self.auth_files.begin(&self.config.vendor)?;
        let tokens = mutation
            .load()?
            .filter(AuthFile::has_credential)
            .or_else(|| self.imported())
            .and_then(|file| file.tokens)
            .ok_or_else(|| AuthError::NotConfigured(AUTH_ID.to_string()))?;

        // Whoever held the lock may have been refreshing the very credential
        // this call was queued to renew.
        if mode == RefreshMode::IfStale && !tokens::is_stale(&tokens, self.config.refresh_leeway) {
            return Ok(());
        }

        let refresh_token = tokens.refresh_token.clone().ok_or_else(|| {
            AuthError::RefreshFailed("the stored credential has no refresh token".into())
        })?;

        // A JSON body, not a form: this endpoint accepts the form encoding for
        // the authorization-code exchange and refuses it for a refresh.
        let response = endpoint::exchange_json(
            &self.http,
            &self.config.token_endpoint,
            &serde_json::json!({
                "grant_type": "refresh_token",
                "refresh_token": refresh_token,
                "client_id": self.config.client_id,
                "scope": self.config.scope_param(),
            }),
        )
        .await?;

        mutation.save(&AuthFile::from_tokens(
            AuthMode::Chatgpt,
            response.into_tokens(Some(refresh_token), tokens.account_id),
        ))?;
        Ok(())
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

impl AuthProvider for CodexAuth {
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
                        .or_else(|| Some(stable_id("openai-access-token", &tokens.access_token))),
                    organization_id: claims.org_id,
                    expires_at: tokens::expires_at(&tokens),
                    ..base
                };
            }
            if let Some(key) = file.api_key {
                return CredentialSnapshot {
                    source: file.auth_mode.as_str().to_string(),
                    account_id: Some(stable_id("openai-api-key", &key)),
                    ..base
                };
            }
        }

        match self.api_key().ok().flatten() {
            Some(key) => CredentialSnapshot {
                source: SOURCE_ENV.to_string(),
                account_id: Some(stable_id("openai-api-key", &key)),
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
                    self.refresh_once(RefreshMode::IfStale)
                        .await
                        .map_err(|err| {
                            AuthError::RefreshFailed(format!(
                                "the ChatGPT access token expired and could not be renewed: {err}"
                            ))
                        })?;
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

    /// An existing codex CLI login is adopted in place of a browser flow, but
    /// only when keke holds no credential of its own — see
    /// [`keke_credentials::Importer`] for why that ordering is the whole point.
    fn login<'a>(&'a self, ui: Arc<dyn LoginUi>) -> AuthFuture<'a, Result<(), AuthError>> {
        Box::pin(async move {
            let stored = self.auth_files.load(&self.config.vendor)?;
            if !stored.is_some_and(|file| file.has_credential())
                && let Some(imported) = self.imported()
            {
                ui.notice("adopting the existing codex CLI login");
                self.auth_files.save(&self.config.vendor, &imported)?;
                return Ok(());
            }

            let listener = if self.config.device_code_only {
                None
            } else {
                match loopback::bind(self.config.callback_port).await {
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

            let tokens = match listener {
                Some(listener) => {
                    loopback::run(&self.http, &self.config, ui.as_ref(), listener).await?
                }
                None => {
                    device::run(&self.http, &self.config, ui.as_ref(), self.delay.as_ref()).await?
                }
            };

            self.save(tokens)
        })
    }

    fn refresh_after_unauthorized(&self) -> AuthFuture<'_, bool> {
        Box::pin(async move { self.refresh_once(RefreshMode::Force).await.is_ok() })
    }

    /// Clears only the credential this plugin minted. An API key the deployment
    /// supplied is not ours to revoke, and deleting it would make `logout` a
    /// way to lose a secret keke never issued. The codex CLI's own file is
    /// likewise untouched — keke never wrote it and must not delete it.
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
    use crate::test_support::chatgpt;
    use crate::test_support::store_tokens;
    use crate::test_support::stored_tokens;
    use keke_credentials::MemoryStore;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::header;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    fn codex_cli_login() -> serde_json::Value {
        serde_json::json!({
            "OPENAI_API_KEY": serde_json::Value::Null,
            "tokens": {
                "id_token": "header.payload.signature",
                "access_token": "from-codex-cli",
                "refresh_token": "codex-cli-refresh",
                "account_id": "acct-9",
            },
            "last_refresh": "2026-08-21T10:00:00Z",
        })
    }

    #[test]
    fn the_registry_key_is_stable() {
        let home = Home::new();
        let auth = chatgpt(
            &home,
            &Arc::new(MemoryStore::new()),
            CodexAuthConfig::default(),
        );
        assert_eq!(auth.id(), "codex");
    }

    #[test]
    fn the_credential_lands_in_a_file_named_after_the_vendor() {
        let home = Home::new();
        let auth = chatgpt(
            &home,
            &Arc::new(MemoryStore::new()),
            CodexAuthConfig::default(),
        );
        let path = auth
            .auth_files
            .path(&auth.config().vendor)
            .expect("path")
            .to_string();
        assert!(path.ends_with("auth.codex.json"), "{path}");
    }

    #[test]
    fn the_issuer_and_client_id_are_defaults_a_deployment_can_replace() {
        let default = CodexAuthConfig::default();
        assert_eq!(default.issuer, "https://auth.openai.com");
        assert_eq!(
            default.token_endpoint,
            "https://auth.openai.com/oauth/token"
        );

        let private = CodexAuthConfig::new("https://issuer.internal/", "client-9");
        assert_eq!(private.client_id, "client-9");
        assert_eq!(
            private.authorize_endpoint,
            "https://issuer.internal/oauth/authorize"
        );
    }

    #[test]
    fn a_stable_id_is_derived_and_not_the_secret() {
        let id = stable_id("openai-api-key", "sk-super-secret");
        assert_eq!(id.len(), 36);
        assert!(!id.contains("secret"));
        assert_eq!(id, stable_id("openai-api-key", "sk-super-secret"));
        assert_ne!(id, stable_id("openai-access-token", "sk-super-secret"));
    }

    #[tokio::test]
    async fn a_blank_stored_credential_is_not_a_credential() {
        let home = Home::new();
        let store = Arc::new(MemoryStore::new());
        let auth = chatgpt(&home, &store, CodexAuthConfig::default());
        store_tokens(&auth, "   ".into(), None);
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
        let auth = chatgpt(&home, &store, CodexAuthConfig::default());
        store_tokens(
            &auth,
            jwt::encode_unsigned(r#"{"exp":4102444800,"sub":"user-7","org_id":"org-3"}"#),
            Some("refresh"),
        );

        let snapshot = auth.snapshot();
        assert_eq!(snapshot.auth_id, "codex");
        assert_eq!(snapshot.source, "chatgpt");
        assert_eq!(snapshot.account_id.as_deref(), Some("user-7"));
        assert_eq!(snapshot.organization_id.as_deref(), Some("org-3"));
        assert_eq!(snapshot.expires_at, Some(4102444800));
    }

    #[tokio::test]
    async fn an_openai_api_key_from_the_environment_is_the_fallback() {
        let home = Home::new();
        let store = Arc::new(MemoryStore::new());
        let auth = chatgpt(&home, &store, CodexAuthConfig::default());
        store
            .save(&auth.config.api_key_ref, "sk-key-1")
            .expect("save");

        let headers = auth.headers().await.expect("headers");
        assert_eq!(
            headers.iter().collect::<Vec<_>>(),
            vec![("authorization", "Bearer sk-key-1")]
        );
        assert_eq!(auth.snapshot().source, "env");
        assert!(auth.has_usable_credential());
    }

    #[tokio::test]
    async fn logout_clears_the_minted_credential_but_not_a_supplied_key() {
        let home = Home::new();
        let store = Arc::new(MemoryStore::new());
        let auth = chatgpt(&home, &store, CodexAuthConfig::default());
        store_tokens(&auth, "access".into(), None);
        store
            .save(&auth.config.api_key_ref, "sk-key-1")
            .expect("save");

        auth.logout().await.expect("logout");
        assert!(stored_tokens(&auth).is_none());
        assert_eq!(
            store
                .load(&auth.config.api_key_ref)
                .expect("load")
                .as_deref(),
            Some("sk-key-1")
        );
    }

    #[tokio::test]
    async fn an_existing_codex_cli_login_is_used_instead_of_a_browser_flow() {
        let home = Home::new();
        let importer = home.with_codex_cli_login(codex_cli_login());
        let auth = CodexAuth::new(
            Arc::new(MemoryStore::new()),
            home.auth_files(),
            CodexAuthConfig::default(),
        )
        .with_importer(importer);
        let before = home.codex_cli_bytes();

        let ui = RecordingUi::new();
        auth.login(ui.clone()).await.expect("login");

        assert!(
            ui.browser_urls().is_empty(),
            "an adoptable login must not open a browser"
        );
        let stored = stored_tokens(&auth).expect("tokens");
        assert_eq!(stored.access_token, "from-codex-cli");
        assert_eq!(stored.refresh_token.as_deref(), Some("codex-cli-refresh"));
        assert_eq!(
            home.codex_cli_bytes(),
            before,
            "importing must never write to the codex CLI's file"
        );
    }

    #[tokio::test]
    async fn an_explicit_login_result_wins_over_an_available_import() {
        let home = Home::new();
        let importer = home.with_codex_cli_login(codex_cli_login());
        let auth = CodexAuth::new(
            Arc::new(MemoryStore::new()),
            home.auth_files(),
            CodexAuthConfig::default(),
        )
        .with_importer(importer);
        store_tokens(&auth, "from-keke-login".into(), None);

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
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "access-2",
                "refresh_token": "refresh-2",
                "expires_in": 3600,
            })))
            .mount(&server)
            .await;

        let home = Home::new();
        let store = Arc::new(MemoryStore::new());
        let auth = chatgpt(
            &home,
            &store,
            CodexAuthConfig::new(server.uri(), "client-1"),
        );
        store_tokens(&auth, "access-1".into(), Some("refresh-1"));

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
    async fn an_expired_access_token_is_refreshed_before_headers_are_produced() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "access-2",
                "expires_in": 3600,
            })))
            .mount(&server)
            .await;

        let home = Home::new();
        let store = Arc::new(MemoryStore::new());
        let auth = chatgpt(
            &home,
            &store,
            CodexAuthConfig::new(server.uri(), "client-1"),
        );
        let expired = jwt::encode_unsigned(&format!(r#"{{"exp":{}}}"#, tokens::now() - 30));
        store_tokens(&auth, expired, Some("refresh-1"));

        let headers = auth.headers().await.expect("headers");
        assert_eq!(
            headers.iter().collect::<Vec<_>>(),
            vec![("authorization", "Bearer access-2")]
        );
        assert_eq!(server.received_requests().await.expect("requests").len(), 1);
    }

    #[tokio::test]
    async fn a_refresh_is_posted_as_json() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .and(header("content-type", "application/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "access-2",
                "expires_in": 3600,
            })))
            .mount(&server)
            .await;

        let home = Home::new();
        let store = Arc::new(MemoryStore::new());
        let auth = chatgpt(
            &home,
            &store,
            CodexAuthConfig::new(server.uri(), "client-1"),
        );
        let expired = jwt::encode_unsigned(&format!(r#"{{"exp":{}}}"#, tokens::now() - 30));
        store_tokens(&auth, expired, Some("refresh-1"));

        assert!(
            auth.refresh_after_unauthorized().await,
            "this endpoint refuses a form-encoded refresh"
        );
    }

    #[tokio::test]
    async fn a_refusal_reaches_the_caller_with_the_reason_the_issuer_gave() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": "invalid_grant",
                "error_description": "refresh token revoked",
            })))
            .mount(&server)
            .await;

        let home = Home::new();
        let store = Arc::new(MemoryStore::new());
        let auth = chatgpt(
            &home,
            &store,
            CodexAuthConfig::new(server.uri(), "client-1"),
        );
        let expired = jwt::encode_unsigned(&format!(r#"{{"exp":{}}}"#, tokens::now() - 30));
        store_tokens(&auth, expired, Some("refresh-1"));

        let Err(AuthError::RefreshFailed(detail)) = auth.headers().await else {
            panic!("an expired credential the issuer refuses to renew must fail");
        };
        assert!(
            detail.contains("refresh token revoked"),
            "a person cannot tell a revoked credential from a blocked network without the reason: {detail}"
        );
    }

    #[tokio::test]
    async fn a_refused_refresh_is_reported_rather_than_retried_forever() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_json(serde_json::json!({ "error": "invalid_grant" })),
            )
            .mount(&server)
            .await;

        let home = Home::new();
        let store = Arc::new(MemoryStore::new());
        let auth = chatgpt(
            &home,
            &store,
            CodexAuthConfig::new(server.uri(), "client-1"),
        );
        let expired = jwt::encode_unsigned(&format!(r#"{{"exp":{}}}"#, tokens::now() - 30));
        store_tokens(&auth, expired, Some("refresh-1"));

        assert!(matches!(
            auth.headers().await,
            Err(AuthError::RefreshFailed(_))
        ));
    }
}
