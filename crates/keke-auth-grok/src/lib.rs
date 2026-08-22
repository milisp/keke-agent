//! xAI authentication: OAuth2 over a loopback redirect, RFC 8628 device code,
//! and a long-lived API key.
//!
//! Nothing here caches a resolved credential across operations. `headers` reads
//! the store on every request so a refresh — which may have happened in another
//! task, for another request — reaches the next call without a restart.

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
use sha2::Digest as _;
use sha2::Sha256;
use tokio::sync::Mutex;

pub use config::DEFAULT_API_KEY_REF;
pub use config::DEFAULT_CLIENT_ID;
pub use config::DEFAULT_ISSUER;
pub use config::DEFAULT_TOKENS_REF;
pub use config::GrokAuthConfig;

use crate::device::Delay;
use crate::device::TokioDelay;
use crate::tokens::SOURCE_ENV;
use crate::tokens::StoredTokens;

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
    store: Arc<dyn CredentialStore>,
    http: reqwest::Client,
    refresh: Mutex<Refresh>,
    generation: AtomicU64,
    delay: Arc<dyn Delay>,
}

impl GrokAuth {
    pub fn new(store: Arc<dyn CredentialStore>, config: GrokAuthConfig) -> Self {
        Self {
            config,
            store,
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
    pub fn with_defaults(store: Arc<dyn CredentialStore>) -> Self {
        Self::new(store, GrokAuthConfig::default())
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

    fn stored_tokens(&self) -> Result<Option<StoredTokens>, AuthError> {
        let Some(raw) = self.store.load(&self.config.tokens_ref)? else {
            return Ok(None);
        };
        let tokens: StoredTokens = serde_json::from_str(&raw).map_err(|_| {
            AuthError::Other(format!(
                "`{}` does not hold an xAI credential document",
                self.config.tokens_ref
            ))
        })?;
        Ok((!tokens.access_token.trim().is_empty()).then_some(tokens))
    }

    fn save_tokens(&self, tokens: &StoredTokens) -> Result<(), AuthError> {
        let document =
            serde_json::to_string(tokens).map_err(|err| AuthError::Other(err.to_string()))?;
        self.store.save(&self.config.tokens_ref, &document)?;
        Ok(())
    }

    fn api_key(&self) -> Result<Option<String>, AuthError> {
        Ok(self.store.load(&self.config.api_key_ref)?)
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
            .stored_tokens()?
            .ok_or_else(|| AuthError::NotConfigured(AUTH_ID.to_string()))?;
        let refresh_token = current.refresh_token.clone().ok_or_else(|| {
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

        self.save_tokens(&response.into_stored(&current.source, Some(refresh_token)))
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

        if let Some(tokens) = self.stored_tokens().ok().flatten() {
            let claims = jwt::claims(&tokens.access_token).unwrap_or_default();
            return CredentialSnapshot {
                source: tokens.source.clone(),
                account_id: claims
                    .sub
                    .or_else(|| Some(stable_id("xai-access-token", &tokens.access_token))),
                organization_id: claims.org_id,
                expires_at: tokens.expires_at(),
                ..base
            };
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
        self.stored_tokens().ok().flatten().is_some() || self.api_key().ok().flatten().is_some()
    }

    fn headers(&self) -> AuthFuture<'_, Result<AuthHeaders, AuthError>> {
        Box::pin(async move {
            if let Some(tokens) = self.stored_tokens()? {
                if !tokens.is_stale(self.config.refresh_leeway) {
                    return Ok(AuthHeaders::bearer(&tokens.access_token));
                }
                if !self.refresh_once().await {
                    return Err(AuthError::RefreshFailed(
                        "the xAI access token expired and could not be renewed".into(),
                    ));
                }
                let refreshed = self.stored_tokens()?.ok_or_else(|| {
                    AuthError::RefreshFailed("the refreshed credential vanished".into())
                })?;
                return Ok(AuthHeaders::bearer(&refreshed.access_token));
            }

            match self.api_key()? {
                Some(key) => Ok(AuthHeaders::bearer(&key)),
                None => Err(AuthError::NotConfigured(AUTH_ID.to_string())),
            }
        })
    }

    fn login<'a>(&'a self, ui: Arc<dyn LoginUi>) -> AuthFuture<'a, Result<(), AuthError>> {
        Box::pin(async move {
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

            let tokens = match listener {
                Some(listener) => {
                    loopback::run(&self.http, &self.config, ui.as_ref(), listener).await?
                }
                None => {
                    device::run(&self.http, &self.config, ui.as_ref(), self.delay.as_ref()).await?
                }
            };

            self.save_tokens(&tokens)
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
            self.store.delete(&self.config.tokens_ref)?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::RecordingUi;
    use crate::test_support::store_tokens;
    use crate::test_support::xai;
    use keke_credentials::MemoryStore;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    #[test]
    fn the_registry_key_is_stable() {
        let auth = GrokAuth::with_defaults(Arc::new(MemoryStore::new()));
        assert_eq!(auth.id(), "grok");
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
        let store = Arc::new(MemoryStore::new());
        let auth = GrokAuth::with_defaults(store.clone());
        store.save(&auth.config.tokens_ref, "").unwrap();
        store.save(&auth.config.api_key_ref, "   ").unwrap();

        assert!(!auth.has_usable_credential());
        assert!(matches!(
            auth.headers().await,
            Err(AuthError::NotConfigured(_))
        ));
    }

    #[tokio::test]
    async fn a_snapshot_never_carries_the_token() {
        let store = Arc::new(MemoryStore::new());
        let auth = xai(&store, GrokAuthConfig::default());
        store_tokens(
            &store,
            &auth,
            jwt::encode_unsigned(r#"{"exp":4102444800,"sub":"user-7","org_id":"org-3"}"#),
            Some("refresh"),
            tokens::SOURCE_DEVICE_CODE,
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
        let store = Arc::new(MemoryStore::new());
        let auth = xai(&store, GrokAuthConfig::default());
        store.save(&auth.config.api_key_ref, "xai-key-1").unwrap();

        let headers: Vec<_> = auth
            .headers()
            .await
            .unwrap()
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
        let store = Arc::new(MemoryStore::new());
        let auth = xai(&store, GrokAuthConfig::default());
        store_tokens(&store, &auth, "access".into(), None, tokens::SOURCE_OAUTH);
        store.save(&auth.config.api_key_ref, "xai-key-1").unwrap();

        auth.logout().await.unwrap();
        assert!(store.load(&auth.config.tokens_ref).unwrap().is_none());
        assert_eq!(
            store.load(&auth.config.api_key_ref).unwrap().as_deref(),
            Some("xai-key-1")
        );
    }

    #[tokio::test]
    async fn the_login_ui_stub_records_nothing_until_a_flow_runs() {
        let ui = RecordingUi::new();
        assert!(ui.device_codes().is_empty());
        assert!(ui.browser_urls().is_empty());
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

        let store = Arc::new(MemoryStore::new());
        let auth = xai(&store, GrokAuthConfig::new(server.uri(), "client-1"));
        store_tokens(
            &store,
            &auth,
            "access-1".into(),
            Some("refresh-1"),
            tokens::SOURCE_OAUTH,
        );

        let outcomes = tokio::join!(
            auth.refresh_after_unauthorized(),
            auth.refresh_after_unauthorized(),
            auth.refresh_after_unauthorized(),
            auth.refresh_after_unauthorized(),
        );
        assert_eq!(outcomes, (true, true, true, true));
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "four concurrent 401s must renew the credential once"
        );
        assert_eq!(
            auth.stored_tokens().unwrap().unwrap().access_token,
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

        let store = Arc::new(MemoryStore::new());
        let auth = xai(&store, GrokAuthConfig::new(server.uri(), "client-1"));
        store_tokens(
            &store,
            &auth,
            "access-1".into(),
            Some("refresh-1"),
            tokens::SOURCE_OAUTH,
        );

        assert!(auth.refresh_after_unauthorized().await);
        assert!(auth.refresh_after_unauthorized().await);
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
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

        let store = Arc::new(MemoryStore::new());
        let auth = xai(&store, GrokAuthConfig::new(server.uri(), "client-1"));
        let expired = jwt::encode_unsigned(&format!(r#"{{"exp":{}}}"#, tokens::now() - 30));
        store_tokens(
            &store,
            &auth,
            expired,
            Some("refresh-1"),
            tokens::SOURCE_OAUTH,
        );

        let headers = auth.headers().await.unwrap();
        assert_eq!(
            headers.iter().collect::<Vec<_>>(),
            vec![("authorization", "Bearer access-2")]
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_token_that_is_still_good_is_used_as_is() {
        let server = MockServer::start().await;
        let store = Arc::new(MemoryStore::new());
        let auth = xai(&store, GrokAuthConfig::new(server.uri(), "client-1"));
        let fresh = jwt::encode_unsigned(&format!(r#"{{"exp":{}}}"#, tokens::now() + 3600));
        store_tokens(
            &store,
            &auth,
            fresh.clone(),
            Some("refresh-1"),
            tokens::SOURCE_OAUTH,
        );

        let headers = auth.headers().await.unwrap();
        assert_eq!(
            headers.iter().collect::<Vec<_>>(),
            vec![("authorization", format!("Bearer {fresh}").as_str())]
        );
        assert!(
            server.received_requests().await.unwrap().is_empty(),
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

        let store = Arc::new(MemoryStore::new());
        let auth = xai(&store, GrokAuthConfig::new(server.uri(), "client-1"));
        let expired = jwt::encode_unsigned(&format!(r#"{{"exp":{}}}"#, tokens::now() - 30));
        store_tokens(
            &store,
            &auth,
            expired,
            Some("refresh-1"),
            tokens::SOURCE_OAUTH,
        );

        assert!(matches!(
            auth.headers().await,
            Err(AuthError::RefreshFailed(_))
        ));
    }
}
