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
/// Where a grok *login* is spent. A subscription credential — and the free
/// hours that come with it — is not valid at the pay-per-token API, which
/// answers it `403 personal-team-blocked:spending-limit`: the credential is
/// accepted and the account refused, which reads as a billing problem the
/// person does not have.
const GROK_SUBSCRIPTION_BASE_URL: &str = "https://cli-chat-proxy.grok.com/v1";
const CHATGPT_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";

/// Pick the endpoint the stored credential is actually valid at.
///
/// Sending a subscription token to the public API, or an API key to the
/// subscription backend, fails as a 401 that looks like a bad credential rather
/// than like the wrong address.
/// Whether this credential is a login rather than a key.
///
/// The two are spent at different addresses for both vendors, and sending
/// either to the other's fails as an authentication error that names neither
/// the address nor the account.
fn is_subscription(auth: &dyn AuthProvider) -> bool {
    !matches!(auth.snapshot().source.as_str(), "apikey" | "env")
}

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
    /// The plugins' slash commands. Kept as data rather than as a registry
    /// because a command is a prompt file: only a surface that has someone to
    /// type it means anything by it.
    pub commands: Vec<keke_plugin::ResolvedCommand>,
}

impl Composed {
    /// Build the full set of vendors, tools, and extensions.
    /// `approvals` is the surface's answer to an approval request. A surface
    /// that cannot ask passes `None`, and the engine's default — denial — takes
    /// over; the registry is frozen once built, so this has to be decided here
    /// rather than added later.
    pub(crate) fn build(
        home: &keke_config_types::HomeLayout,
        declared: &[keke_config_types::ProviderDeclaration],
        timeouts: keke_config_types::PluginTimeouts,
        approvals: Option<Arc<keke_acp::Approvals>>,
    ) -> Result<Self> {
        // Resolution finds every plugin; this holds back the programs of the
        // ones nobody vouched for. A plugin under the workspace is content the
        // repository controls, and cloning a repository is not consent to run
        // what it ships.
        let (plugins, withheld) = crate::plugins::discover_trusted(home)?;
        crate::plugins::report_withheld(&withheld);
        let home = &home.home;
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
        let grok_base_url = std::env::var("XAI_BASE_URL")
            .ok()
            .filter(|url| !url.trim().is_empty());
        providers
            .register(if is_subscription(grok_auth.as_ref()) {
                // The subscription surface speaks the responses wire, not
                // chat-completions — a login sent to the latter is refused
                // before the model is ever reached.
                crate::declared::wire_provider(
                    ProviderInfo {
                        route: keke_auth_grok::AUTH_ID.to_string(),
                        display_name: "xAI Grok".to_string(),
                        base_url: grok_base_url
                            .unwrap_or_else(|| GROK_SUBSCRIPTION_BASE_URL.to_string()),
                        wire_api: WireApi::Responses,
                        auth_id: Some(keke_auth_grok::AUTH_ID.to_string()),
                        env_key: Some(keke_auth_grok::DEFAULT_API_KEY_REF.to_string()),
                    },
                    grok_auth,
                )
            } else {
                Arc::new(keke_provider_grok::GrokProvider::new(
                    grok_auth,
                    grok_base_url,
                )) as ArcProvider
            })
            .context("registering the grok provider")?;

        let codex_auth: Arc<dyn AuthProvider> = Arc::new(
            keke_auth_codex::CodexAuth::with_defaults(Arc::clone(&credentials), auth_files),
        );
        auth.register(Arc::clone(&codex_auth));
        let codex_is_subscription = is_subscription(codex_auth.as_ref());
        providers
            .register(crate::declared::wire_provider_with(
                ProviderInfo {
                    route: keke_auth_codex::AUTH_ID.to_string(),
                    display_name: "OpenAI Codex".to_string(),
                    base_url: codex_base_url(codex_auth.as_ref()),
                    wire_api: WireApi::Responses,
                    auth_id: Some(keke_auth_codex::AUTH_ID.to_string()),
                    env_key: Some(keke_auth_codex::DEFAULT_API_KEY_REF.to_string()),
                },
                codex_auth,
                codex_is_subscription,
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

        // Runtime plugins register through the same contributor traits as
        // anything compiled in — `keke-core` never learns they exist. Order is
        // priority order for approval reviewers only; for the rest it is what
        // keeps the set identical between runs.
        // The budgets come from configuration rather than from each crate's own
        // constants: how long someone else's program may hold up a turn is a
        // deployment's call (`AGENTS.md` invariant 9).
        keke_skills::install(&mut extensions, &plugins);
        keke_mcp::install_with(&mut extensions, &plugins, timeouts.into());
        keke_hooks::install_with(&mut extensions, &plugins, timeouts);

        // The surface's approval bridge registers last so a plugin hook cannot
        // answer on a person's behalf. A hook may still deny — denial is
        // monotonic and nothing here can undo it.
        if let Some(approvals) = approvals {
            keke_acp::install(&mut extensions, approvals);
        }

        let commands = plugins.commands().cloned().collect();
        Ok(Self {
            credentials,
            auth,
            providers,
            extensions: extensions.build(),
            commands,
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
