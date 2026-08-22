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
use keke_provider_api::ProviderInfo;
use keke_provider_api::ProviderRegistry;
use keke_provider_api::WireApi;

/// Where a ChatGPT subscription's tokens are accepted. An API key is not valid
/// here, and a subscription token is not valid at the public API — so the base
/// URL follows the credential rather than being a single constant.
const CHATGPT_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";

/// Pick the endpoint the stored credential is actually valid at.
///
/// Sending a subscription token to the public API, or an API key to the
/// subscription backend, fails as a 401 that looks like a bad credential rather
/// than like the wrong address.
fn codex_base_url(auth: &dyn AuthProvider) -> String {
    if let Ok(explicit) = std::env::var("OPENAI_BASE_URL") {
        return explicit;
    }
    match auth.snapshot().source.as_str() {
        "apikey" | "env" => OPENAI_BASE_URL.to_string(),
        _ => CHATGPT_BASE_URL.to_string(),
    }
}

/// The keyring service name credentials are filed under.
const CREDENTIAL_SERVICE: &str = "keke";

/// Everything the composition root assembled.
pub(crate) struct Composed {
    /// Kept so a surface can ask whether a key-only endpoint's credential
    /// resolves — through every layer, not just the process environment.
    pub credentials: Arc<dyn CredentialStore>,
    pub auth: AuthRegistry,
    pub providers: ProviderRegistry,
    pub extensions: ExtensionRegistry,
}

impl Composed {
    /// Build the full set of vendors, tools, and extensions.
    pub(crate) fn build(
        home: &keke_paths::AbsPath,
        declared: &[keke_config_types::ProviderDeclaration],
    ) -> Result<Self> {
        let credentials: Arc<dyn CredentialStore> = Arc::new(keke_credentials::standard_store(
            CREDENTIAL_SERVICE,
            keke_credentials::FileStore::new(
                keke_paths::AbsPath::new(home.as_path().join("credentials.json"))
                    .context("resolving the credentials file")?,
            ),
        ));

        // Token sets live in one file per vendor, so two vendors refreshing at
        // once cannot interleave writes and revoking one does not rewrite the
        // others.
        let auth_files = keke_credentials::VendorAuthStore::new(home.clone());

        // --- vendors -------------------------------------------------------
        let mut auth = AuthRegistry::new();
        let mut providers = ProviderRegistry::new();

        let grok_auth: Arc<dyn AuthProvider> = Arc::new(keke_auth_grok::GrokAuth::with_defaults(
            Arc::clone(&credentials),
            auth_files.clone(),
        ));
        auth.register(Arc::clone(&grok_auth));
        providers
            .register(Arc::new(keke_provider_grok::GrokProvider::new(
                grok_auth,
                std::env::var("XAI_BASE_URL").ok(),
            )) as ArcProvider)
            .context("registering the grok provider")?;

        let codex_auth: Arc<dyn AuthProvider> = Arc::new(
            keke_auth_codex::CodexAuth::with_defaults(Arc::clone(&credentials), auth_files),
        );
        auth.register(Arc::clone(&codex_auth));
        providers
            .register(crate::declared::wire_provider(
                ProviderInfo {
                    route: keke_auth_codex::AUTH_ID.to_string(),
                    display_name: "OpenAI Codex".to_string(),
                    base_url: codex_base_url(codex_auth.as_ref()),
                    wire_api: WireApi::Responses,
                    auth_id: Some(keke_auth_codex::AUTH_ID.to_string()),
                    env_key: Some(keke_auth_codex::DEFAULT_API_KEY_REF.to_string()),
                },
                codex_auth,
            ))
            .context("registering the codex provider")?;

        // Declared endpoints register last, so a config file can add a route
        // without being able to shadow a compiled-in vendor by accident —
        // `ProviderRegistry::register` refuses a duplicate rather than
        // silently replacing one.
        for declaration in declared {
            let provider = crate::declared::provider_for(declaration, &credentials)?;
            providers.register(provider).with_context(|| {
                format!("registering declared provider `{}`", declaration.route)
            })?;
        }

        // --- extensions ----------------------------------------------------
        let mut extensions = ExtensionRegistryBuilder::new();
        keke_tools::install(&mut extensions);

        Ok(Self {
            credentials,
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
