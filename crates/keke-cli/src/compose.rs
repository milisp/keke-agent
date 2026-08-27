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
use keke_provider_api::WireApi;
use keke_provider_ollama::OllamaProvider;

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
/// Anthropic's public API. There is no subscription address beside it: keke
/// spends an API key here and nothing else, so unlike the two vendors above
/// this route has one endpoint and one credential shape.
const ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com/v1";
/// The variable the key is read from, and the name surfaces report it under.
/// A reference, never a value — see `keke_auth_api::store`.
const ANTHROPIC_ENV_KEY: &str = "ANTHROPIC_API_KEY";

/// Whether this credential is a login rather than a key.
///
/// The two are spent at different addresses for both vendors, and sending
/// either to the other's fails as an authentication error that names neither
/// the address nor the account. A subscription backend also fixes its own
/// sampling and publishes a richer catalog, so this one answer decides three
/// things at once — which is why it is asked here, in the only place that can
/// see the stored credential.
fn is_subscription(auth: &dyn AuthProvider) -> bool {
    !matches!(auth.snapshot().source.as_str(), "apikey" | "env")
}

/// The environment's override for a vendor's address, when it is set to
/// something other than the empty string.
fn base_url_override(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|url| !url.trim().is_empty())
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
    /// The coordinator behind `spawn_agent`. Handed the session recipe in
    /// `session_builder`, once there is a finished builder to hand it.
    pub subagents: Arc<keke_subagent::SubagentHost>,
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
        catalog_ttl: keke_config_types::ModelCatalogTtl,
        subagent_limits: keke_config_types::SubagentLimits,
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

        // Every compiled-in vendor caches what it serves under keke's home, so
        // opening the interface a dozen times in an afternoon costs one model
        // listing rather than a dozen — and so a picker is still drawn when the
        // vendor cannot be reached at all.
        let catalog = keke_catalog::CatalogCache::new(home, catalog_ttl.get());

        let grok_auth: Arc<dyn AuthProvider> = Arc::new(keke_auth_grok::GrokAuth::with_defaults(
            Arc::clone(&credentials),
            auth_files.clone(),
        ));
        auth.register(Arc::clone(&grok_auth));
        let grok_subscription = is_subscription(grok_auth.as_ref());
        providers
            .register(Arc::new(keke_provider_grok::GrokProvider::new(
                grok_auth,
                keke_provider_grok::Endpoint {
                    base_url: base_url_override("XAI_BASE_URL").unwrap_or_else(|| {
                        if grok_subscription {
                            GROK_SUBSCRIPTION_BASE_URL.to_string()
                        } else {
                            keke_provider_grok::DEFAULT_BASE_URL.to_string()
                        }
                    }),
                    wire_api: if grok_subscription {
                        WireApi::Responses
                    } else {
                        WireApi::ChatCompletions
                    },
                    fixed_sampling: grok_subscription,
                },
                Some(catalog.clone()),
            )) as ArcProvider)
            .context("registering the grok provider")?;

        let codex_auth: Arc<dyn AuthProvider> = Arc::new(
            keke_auth_codex::CodexAuth::with_defaults(Arc::clone(&credentials), auth_files),
        );
        auth.register(Arc::clone(&codex_auth));
        let codex_subscription = is_subscription(codex_auth.as_ref());
        providers
            .register(Arc::new(keke_provider_codex::CodexProvider::new(
                codex_auth,
                keke_provider_codex::Endpoint {
                    base_url: base_url_override("OPENAI_BASE_URL").unwrap_or_else(|| {
                        if codex_subscription {
                            CHATGPT_BASE_URL.to_string()
                        } else {
                            OPENAI_BASE_URL.to_string()
                        }
                    }),
                    fixed_sampling: codex_subscription,
                },
                Some(catalog.clone()),
            )) as ArcProvider)
            .context("registering the codex provider")?;

        // Anthropic has no login flow to register: `auth_id: None` is what
        // tells every surface to talk about the key rather than offer `keke
        // login`, which for this vendor would have nothing to open.
        let anthropic_key = keke_auth_api::CredentialRef::new(ANTHROPIC_ENV_KEY)
            .context("the anthropic credential name")?;
        providers
            .register(crate::declared::wire_provider(
                keke_provider_api::ProviderInfo {
                    route: "anthropic".to_string(),
                    display_name: "Anthropic".to_string(),
                    base_url: base_url_override("ANTHROPIC_BASE_URL")
                        .unwrap_or_else(|| ANTHROPIC_BASE_URL.to_string()),
                    wire_api: WireApi::Messages,
                    auth_id: None,
                    env_key: Some(ANTHROPIC_ENV_KEY.to_string()),
                },
                Arc::new(crate::api_key::ApiKeyAuth::with_header(
                    anthropic_key,
                    crate::api_key::KeyHeader::XApiKey,
                    Arc::clone(&credentials),
                )),
            ))
            .context("registering the anthropic provider")?;

        // Ollama: local endpoint, no auth required by default, ChatCompletions wire.
        providers
            .register(Arc::new(OllamaProvider::new(
                Arc::new(crate::api_key::NoAuth),
                keke_provider_ollama::Endpoint {
                    base_url: base_url_override("OLLAMA_BASE_URL")
                        .unwrap_or_else(|| keke_provider_ollama::DEFAULT_BASE_URL.to_string()),
                    wire_api: WireApi::ChatCompletions,
                },
                Some(catalog.clone()),
            )) as ArcProvider)
            .context("registering the ollama provider")?;

        for declaration in declared {
            let provider = crate::declared::provider_for_cached(
                declaration,
                &credentials,
                Some(catalog.clone()),
            )?;
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
        // Registered before the plugin packs so a plugin can shadow
        // `spawn_agent` with its own, the same way it can shadow a built-in.
        let subagents = keke_subagent::install(&mut extensions, subagent_limits);
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
            subagents,
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
