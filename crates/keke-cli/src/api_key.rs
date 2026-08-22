//! Authentication for endpoints that want nothing but an API key.
//!
//! A config-declared provider — an ollama box, an OpenAI-compatible gateway —
//! has no login flow. It reads a credential by name and sends it as a bearer
//! token. That is the whole of its authentication, and it needs no vendor crate.
//!
//! Such a provider is deliberately **not** registered in the `AuthRegistry`.
//! `AuthProvider::id` returns `&'static str`, so a registry entry needs a name
//! known at compile time, while a declared route's name is read from a file at
//! startup. Rather than leak a string to satisfy that, a declared provider
//! reports `auth_id: None` and carries its credential itself; surfaces that want
//! to report on it read `ProviderInfo::env_key` instead.

use std::sync::Arc;

use keke_auth_api::AuthError;
use keke_auth_api::AuthFuture;
use keke_auth_api::AuthHeaders;
use keke_auth_api::AuthProvider;
use keke_auth_api::CredentialRef;
use keke_auth_api::CredentialSnapshot;
use keke_auth_api::CredentialStore;
use keke_auth_api::LoginUi;

/// Sends a stored API key as a bearer token.
pub(crate) struct ApiKeyAuth {
    key: CredentialRef,
    store: Arc<dyn CredentialStore>,
}

impl ApiKeyAuth {
    pub(crate) fn new(key: CredentialRef, store: Arc<dyn CredentialStore>) -> Self {
        Self { key, store }
    }

    /// Read the key, treating a store failure as absence.
    ///
    /// A backend that cannot be reached is reported by the store's own warning;
    /// turning it into an error here would make every provider that merely
    /// *could* use a key fail to construct.
    fn value(&self) -> Option<String> {
        self.store.load(&self.key).ok().flatten()
    }
}

impl AuthProvider for ApiKeyAuth {
    fn id(&self) -> &'static str {
        // Never used as a registry key: see the module comment.
        "api-key"
    }

    fn snapshot(&self) -> CredentialSnapshot {
        CredentialSnapshot {
            auth_id: self.key.to_string(),
            source: "env".to_string(),
            ..CredentialSnapshot::default()
        }
    }

    fn has_usable_credential(&self) -> bool {
        self.value().is_some()
    }

    fn headers(&self) -> AuthFuture<'_, Result<AuthHeaders, AuthError>> {
        Box::pin(async move {
            // Read per call rather than at construction: a key exported after
            // keke started, or rotated mid-session, reaches the next request.
            self.value()
                .map(|key| AuthHeaders::bearer(&key))
                .ok_or_else(|| AuthError::NotConfigured(self.key.to_string()))
        })
    }

    fn login<'a>(&'a self, _ui: Arc<dyn LoginUi>) -> AuthFuture<'a, Result<(), AuthError>> {
        Box::pin(async move {
            Err(AuthError::Other(format!(
                "this endpoint authenticates with the `{}` environment variable; \
                 export it rather than running `keke login`",
                self.key
            )))
        })
    }

    fn refresh_after_unauthorized(&self) -> AuthFuture<'_, bool> {
        // An API key that was rejected will be rejected again. Reporting a
        // refresh here would make the engine retry a request that cannot work.
        Box::pin(async { false })
    }

    fn logout(&self) -> AuthFuture<'_, Result<(), AuthError>> {
        Box::pin(async move {
            Err(AuthError::Other(format!(
                "nothing stored: unset `{}` to remove this credential",
                self.key
            )))
        })
    }
}

/// Sends no credentials at all.
///
/// A local endpoint — an ollama server on the same machine — needs none, and
/// demanding one would make the most common declared provider unusable.
pub(crate) struct NoAuth;

impl AuthProvider for NoAuth {
    fn id(&self) -> &'static str {
        "none"
    }

    fn snapshot(&self) -> CredentialSnapshot {
        CredentialSnapshot {
            auth_id: "none".to_string(),
            source: "none".to_string(),
            ..CredentialSnapshot::default()
        }
    }

    fn headers(&self) -> AuthFuture<'_, Result<AuthHeaders, AuthError>> {
        Box::pin(async { Ok(AuthHeaders::new()) })
    }

    fn login<'a>(&'a self, _ui: Arc<dyn LoginUi>) -> AuthFuture<'a, Result<(), AuthError>> {
        Box::pin(async {
            Err(AuthError::Other(
                "this endpoint needs no credentials".to_string(),
            ))
        })
    }

    fn refresh_after_unauthorized(&self) -> AuthFuture<'_, bool> {
        Box::pin(async { false })
    }

    fn logout(&self) -> AuthFuture<'_, Result<(), AuthError>> {
        Box::pin(async { Ok(()) })
    }
}

#[cfg(test)]
mod tests {
    use keke_credentials::MemoryStore;

    use super::*;

    fn auth(store: Arc<MemoryStore>) -> ApiKeyAuth {
        ApiKeyAuth::new(
            CredentialRef::new("NVIDIA_API_KEY").expect("valid reference"),
            store,
        )
    }

    #[tokio::test]
    async fn a_stored_key_becomes_a_bearer_header() {
        let store = Arc::new(MemoryStore::new());
        let name = CredentialRef::new("NVIDIA_API_KEY").expect("valid reference");
        store.save(&name, "nv-secret").expect("saves");

        let auth = auth(Arc::clone(&store));
        assert!(auth.has_usable_credential());

        let headers = auth.headers().await.expect("headers");
        let rendered: Vec<(&str, &str)> = headers.iter().collect();
        assert_eq!(rendered, vec![("authorization", "Bearer nv-secret")]);
    }

    #[tokio::test]
    async fn a_missing_key_names_the_variable_to_export() {
        let auth = auth(Arc::new(MemoryStore::new()));
        assert!(!auth.has_usable_credential());

        let error = auth.headers().await.expect_err("not configured");
        assert!(error.to_string().contains("NVIDIA_API_KEY"), "{error}");
    }

    /// The key is read per request, so one exported after keke started, or
    /// rotated mid-session, takes effect without a restart.
    #[tokio::test]
    async fn the_key_is_read_again_for_every_request() {
        let store = Arc::new(MemoryStore::new());
        let name = CredentialRef::new("NVIDIA_API_KEY").expect("valid reference");
        let auth = auth(Arc::clone(&store));

        assert!(auth.headers().await.is_err());
        store.save(&name, "arrived-late").expect("saves");

        let headers = auth.headers().await.expect("headers");
        assert!(
            headers
                .iter()
                .any(|(_, value)| value.contains("arrived-late"))
        );
    }

    /// A rejected API key will be rejected again; claiming a refresh would make
    /// the engine retry a request that cannot succeed.
    #[tokio::test]
    async fn a_rejected_key_never_claims_to_refresh() {
        let store = Arc::new(MemoryStore::new());
        let name = CredentialRef::new("NVIDIA_API_KEY").expect("valid reference");
        store.save(&name, "wrong").expect("saves");

        assert!(!auth(store).refresh_after_unauthorized().await);
    }

    #[tokio::test]
    async fn login_points_at_the_environment_variable_instead() {
        struct Silent;
        impl LoginUi for Silent {
            fn open_browser(&self, _url: &str) {}
            fn show_device_code(&self, _code: &str, _uri: &str) {}
        }

        let error = auth(Arc::new(MemoryStore::new()))
            .login(Arc::new(Silent))
            .await
            .expect_err("no login flow");
        assert!(error.to_string().contains("NVIDIA_API_KEY"), "{error}");
    }
}
