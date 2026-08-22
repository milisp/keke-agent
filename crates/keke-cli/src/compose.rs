//! The composition root.
//!
//! This is the only place in the workspace that names a vendor. Every
//! `install()` call and every provider registration happens here, which is what
//! keeps `keke-core` free of vendor knowledge — a rule
//! `scripts/check-layering.py` enforces rather than trusts.
//!
//! Adding a vendor is two lines: register its `AuthProvider`, register its
//! `ModelProvider`. Nothing else in the workspace changes.

use std::sync::Arc;

use anyhow::Context as _;
use anyhow::Result;
use keke_auth_api::AuthProvider;
use keke_auth_api::AuthRegistry;
use keke_auth_api::CredentialStore;
use keke_plugin_api::ExtensionRegistry;
use keke_plugin_api::ExtensionRegistryBuilder;
use keke_provider_api::ArcProvider;
use keke_provider_api::ProviderRegistry;

/// The keyring service name credentials are filed under.
const CREDENTIAL_SERVICE: &str = "keke";

/// Everything the composition root assembled.
pub(crate) struct Composed {
    pub auth: AuthRegistry,
    pub providers: ProviderRegistry,
    pub extensions: ExtensionRegistry,
}

impl Composed {
    /// Build the full set of vendors, tools, and extensions.
    pub(crate) fn build(home: &keke_paths::AbsPath) -> Result<Self> {
        let credentials: Arc<dyn CredentialStore> = Arc::new(keke_credentials::standard_store(
            CREDENTIAL_SERVICE,
            keke_credentials::FileStore::new(
                keke_paths::AbsPath::new(home.as_path().join("credentials.json"))
                    .context("resolving the credentials file")?,
            ),
        ));

        // --- vendors -------------------------------------------------------
        let mut auth = AuthRegistry::new();
        let xai_auth: Arc<dyn AuthProvider> = Arc::new(keke_auth_grok::GrokAuth::with_defaults(
            Arc::clone(&credentials),
        ));
        auth.register(Arc::clone(&xai_auth));

        let mut providers = ProviderRegistry::new();
        let xai: ArcProvider = Arc::new(keke_provider_grok::GrokProvider::new(
            Arc::clone(&xai_auth),
            std::env::var("XAI_BASE_URL").ok(),
        ));
        providers
            .register(xai)
            .context("registering the xAI provider")?;

        // --- extensions ----------------------------------------------------
        let mut extensions = ExtensionRegistryBuilder::new();
        keke_tools::install(&mut extensions);

        Ok(Self {
            auth,
            providers,
            extensions: extensions.build(),
        })
    }

    /// The auth provider backing a model provider route, when there is one.
    ///
    /// Looked up through `ProviderInfo::auth_id` rather than hardcoded, so a
    /// vendor that shares another's credentials needs no special case here.
    pub(crate) fn auth_for(&self, route: &str) -> Option<Arc<dyn AuthProvider>> {
        let provider = self.providers.get(route).ok()?;
        let id = provider.info().auth_id.clone()?;
        self.auth.get(&id)
    }
}
