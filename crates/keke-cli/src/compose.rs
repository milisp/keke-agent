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
/// Only consulted for an instance that did not say. A declaration naming an
/// address and a wire has already answered the question, and sniffing the
/// stored credential to answer it again is how a dead login came to pin a
/// session to the subscription proxy with a usable API key sitting unread
/// beside it.
fn is_subscription(auth: &dyn AuthProvider) -> bool {
    !matches!(auth.snapshot().source.as_str(), "apikey" | "env")
}

/// The compiled-in implementations a declaration may name as its `kind`.
///
/// This list is the reason `kind` is a plain string in `keke-config-types`:
/// the config crate must not learn which vendors exist, and the composition
/// root is the only place allowed to.
const KNOWN_KINDS: &[&str] = &["grok", "codex", "anthropic", "ollama", "openai-compatible"];

/// The environment's override for a vendor's address, when it is set to
/// something other than the empty string.
fn base_url_override(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|url| !url.trim().is_empty())
}

/// The keyring service name credentials are filed under.
const CREDENTIAL_SERVICE: &str = "keke";

/// A built-in instance, expressed as the declaration a person could have
/// written themselves.
///
/// Defaults go through the same path as configuration rather than beside it:
/// a default that took a shortcut would be a shape nobody can reproduce in
/// `config.toml`, and every field of it would then be one more thing that can
/// only be changed by rebuilding (`AGENTS.md` invariant 9).
fn builtin(route: &str, kind: &str) -> keke_config_types::ProviderDeclaration {
    keke_config_types::ProviderDeclaration {
        route: route.to_string(),
        kind: Some(kind.to_string()),
        account: None,
        display_name: None,
        base_url: None,
        wire: None,
        env_key: None,
        default_model: None,
        ca_cert_path: None,
        proxy: None,
        proxy_username: None,
        proxy_password_env_key: None,
        headers: std::collections::BTreeMap::new(),
    }
}

/// The routes that exist when nobody has configured anything.
///
/// One instance per vendor, named after it. A second instance of the same
/// vendor — `[providers.xai]` beside `[providers.grok]` — is configuration's
/// business, not a default's.
fn builtin_defaults() -> Vec<keke_config_types::ProviderDeclaration> {
    vec![
        builtin("grok", "grok"),
        builtin("codex", "codex"),
        builtin("anthropic", "anthropic"),
        builtin("ollama", "ollama"),
    ]
}

/// What every instance is built from: the credentials, the per-vendor auth
/// providers, and the shared model-listing cache.
struct Vendors {
    credentials: Arc<dyn CredentialStore>,
    /// One auth provider per (vendor, account), keyed by [`auth_id`].
    ///
    /// Shared rather than rebuilt per instance: two routes on one account must
    /// share a refresh, or each would race the other's rotation and read the
    /// loser's token as revoked. Two *accounts* are two credentials and have
    /// nothing to contend over, which is why the account is part of the key.
    auths: std::collections::BTreeMap<String, Arc<dyn AuthProvider>>,
    catalog: keke_catalog::CatalogCache,
}

/// What an instance's credentials are filed under.
///
/// The bare vendor name for the account in force, so a deployment that never
/// heard of accounts sees the id it always saw; `vendor/account` otherwise.
fn auth_id(kind: &str, account: Option<&str>) -> String {
    match account {
        Some(account) => format!("{kind}/{account}"),
        None => kind.to_string(),
    }
}

impl Vendors {
    /// Turn one declaration into a registered route.
    ///
    /// This match is the whole of keke's vendor knowledge. It is here, in the
    /// composition root, because `AGENTS.md` invariant 1 says a vendor name may
    /// appear nowhere else.
    fn instance(
        &self,
        declaration: &keke_config_types::ProviderDeclaration,
    ) -> Result<ArcProvider> {
        let route = declaration.route.as_str();
        match declaration.kind.as_deref() {
            None => Ok(crate::declared::provider_for_cached(
                declaration,
                &self.credentials,
                Some(self.catalog.clone()),
            )?),
            Some("grok") => self.grok(declaration),
            Some("codex") => self.codex(declaration),
            Some("anthropic") => self.anthropic(declaration),
            Some("ollama") => Ok(self.ollama(declaration)),
            // A declared endpoint that speaks a standard wire is what the
            // kindless path already builds; naming it explicitly reads better
            // in a config file than leaving `kind` out and hoping.
            Some("openai-compatible") => Ok(crate::declared::provider_for_cached(
                declaration,
                &self.credentials,
                Some(self.catalog.clone()),
            )?),
            Some(unknown) => Err(crate::declared::DeclarationError::UnknownKind {
                route: route.to_string(),
                kind: unknown.to_string(),
                known: KNOWN_KINDS.join(", "),
            }
            .into()),
        }
    }

    /// xAI, at whichever of its two addresses this instance names.
    ///
    /// Sniffing the stored credential is the *fallback*, not the rule: an
    /// instance that stated a `base_url` and a `wire` has already said which
    /// address it is, and nothing about the credential may override it.
    fn grok(&self, declaration: &keke_config_types::ProviderDeclaration) -> Result<ArcProvider> {
        let auth = self.auth_for(declaration, "grok")?;
        let subscription = is_subscription(auth.as_ref());
        let stated_wire = declaration.wire.map(crate::declared::wire_api);
        Ok(Arc::new(keke_provider_grok::GrokProvider::new(
            auth,
            keke_provider_grok::Endpoint {
                route: declaration.route.clone(),
                display_name: Self::display_name(declaration, "Grok"),
                auth_id: auth_id("grok", declaration.account.as_deref()),
                base_url: self.address(
                    declaration,
                    "XAI_BASE_URL",
                    if subscription {
                        GROK_SUBSCRIPTION_BASE_URL
                    } else {
                        keke_provider_grok::DEFAULT_BASE_URL
                    },
                ),
                wire_api: stated_wire.unwrap_or(if subscription {
                    WireApi::Responses
                } else {
                    WireApi::ChatCompletions
                }),
                // A subscription proxy refuses a request that names a reply
                // budget or a temperature. Which one an instance is follows
                // its wire when it stated one, since only the proxy speaks
                // `responses` for this vendor.
                fixed_sampling: stated_wire.map_or(subscription, |wire| wire == WireApi::Responses),
            },
            Some(self.catalog.clone()),
        )) as ArcProvider)
    }

    /// OpenAI, at either the ChatGPT backend or the public API.
    fn codex(&self, declaration: &keke_config_types::ProviderDeclaration) -> Result<ArcProvider> {
        let auth = self.auth_for(declaration, "codex")?;
        let subscription = is_subscription(auth.as_ref());
        let stated = declaration.base_url.is_some();
        Ok(Arc::new(keke_provider_codex::CodexProvider::new(
            auth,
            keke_provider_codex::Endpoint {
                route: declaration.route.clone(),
                display_name: Self::display_name(declaration, "ChatGPT"),
                auth_id: auth_id("codex", declaration.account.as_deref()),
                base_url: self.address(
                    declaration,
                    "OPENAI_BASE_URL",
                    if subscription {
                        CHATGPT_BASE_URL
                    } else {
                        OPENAI_BASE_URL
                    },
                ),
                // An instance pointed at an address of its own is not the
                // ChatGPT backend, whatever credential happens to be stored.
                fixed_sampling: if stated { false } else { subscription },
            },
            Some(self.catalog.clone()),
        )) as ArcProvider)
    }

    /// Anthropic has no login flow to register: `auth_id: None` is what tells
    /// every surface to talk about the key rather than offer `keke login`,
    /// which for this vendor would have nothing to open.
    fn anthropic(
        &self,
        declaration: &keke_config_types::ProviderDeclaration,
    ) -> Result<ArcProvider> {
        let env_key = declaration.env_key.as_deref().unwrap_or(ANTHROPIC_ENV_KEY);
        let key =
            keke_auth_api::CredentialRef::new(env_key).context("the anthropic credential name")?;
        Ok(crate::declared::wire_provider(
            keke_provider_api::ProviderInfo {
                route: declaration.route.clone(),
                display_name: declaration
                    .display_name
                    .clone()
                    .unwrap_or_else(|| "Anthropic".to_string()),
                base_url: self.address(declaration, "ANTHROPIC_BASE_URL", ANTHROPIC_BASE_URL),
                wire_api: declaration
                    .wire
                    .map_or(WireApi::Messages, crate::declared::wire_api),
                auth_id: None,
                env_key: Some(env_key.to_string()),
            },
            Arc::new(crate::api_key::ApiKeyAuth::with_header(
                key,
                crate::api_key::KeyHeader::XApiKey,
                Arc::clone(&self.credentials),
            )),
        ))
    }

    /// A local ollama box: no auth, ChatCompletions wire.
    fn ollama(&self, declaration: &keke_config_types::ProviderDeclaration) -> ArcProvider {
        Arc::new(OllamaProvider::new(
            Arc::new(crate::api_key::NoAuth),
            keke_provider_ollama::Endpoint {
                route: declaration.route.clone(),
                display_name: Self::display_name(declaration, "Ollama"),
                base_url: self.address(
                    declaration,
                    "OLLAMA_BASE_URL",
                    keke_provider_ollama::DEFAULT_BASE_URL,
                ),
                wire_api: declaration
                    .wire
                    .map_or(WireApi::ChatCompletions, crate::declared::wire_api),
            },
            Some(self.catalog.clone()),
        )) as ArcProvider
    }

    /// The auth provider this instance authenticates through.
    ///
    /// Present by construction: `build` created one for every (kind, account)
    /// pair the declarations named before any instance was made.
    fn auth_for(
        &self,
        declaration: &keke_config_types::ProviderDeclaration,
        kind: &str,
    ) -> Result<Arc<dyn AuthProvider>> {
        let id = auth_id(kind, declaration.account.as_deref());
        self.auths.get(&id).cloned().ok_or_else(|| {
            anyhow::anyhow!(
                "provider `{}`: no credentials registered for account `{}`",
                declaration.route,
                declaration.account.as_deref().unwrap_or("<active>")
            )
        })
    }

    /// What surfaces call this instance: what it declared, else the vendor's
    /// own name. A second instance left unnamed shows the vendor's name twice,
    /// which is the person's to fix by naming it — not ours to fix by
    /// inventing a suffix they never wrote.
    fn display_name(declaration: &keke_config_types::ProviderDeclaration, vendor: &str) -> String {
        declaration
            .display_name
            .clone()
            .unwrap_or_else(|| vendor.to_string())
    }

    /// Where an instance sends its requests: what it declared, else what the
    /// environment overrides, else the vendor's own address.
    ///
    /// Configuration outranks the environment variable rather than the other
    /// way round. The variable is a single global, so letting it win would
    /// mean one exported `XAI_BASE_URL` silently redirecting every xAI
    /// instance — including the one a person configured precisely so it would
    /// go somewhere else.
    fn address(
        &self,
        declaration: &keke_config_types::ProviderDeclaration,
        env: &str,
        fallback: &str,
    ) -> String {
        declaration
            .base_url
            .clone()
            .or_else(|| base_url_override(env))
            .unwrap_or_else(|| fallback.to_string())
    }
}

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

        // Every instance that will be built, so the auth providers exist
        // before anything asks for one. Declarations first, for the same
        // reason they are registered first: a named route configures the
        // built-in of that name rather than colliding with it.
        let all: Vec<keke_config_types::ProviderDeclaration> = declared
            .iter()
            .cloned()
            .chain(
                builtin_defaults()
                    .into_iter()
                    .filter(|default| !declared.iter().any(|d| d.route == default.route)),
            )
            .collect();

        let mut auths: std::collections::BTreeMap<String, Arc<dyn AuthProvider>> =
            std::collections::BTreeMap::new();
        for declaration in &all {
            let account = declaration.account.clone();
            let provider: Arc<dyn AuthProvider> = match declaration.kind.as_deref() {
                Some("grok") => Arc::new(
                    keke_auth_grok::GrokAuth::with_defaults(
                        Arc::clone(&credentials),
                        auth_files.clone(),
                    )
                    .as_account(account.clone()),
                ),
                Some("codex") => Arc::new(
                    keke_auth_codex::CodexAuth::with_defaults(
                        Arc::clone(&credentials),
                        auth_files.clone(),
                    )
                    .as_account(account.clone()),
                ),
                // Nothing else has a login flow, so there is nothing for
                // `keke login` to name and nothing to register.
                _ => continue,
            };
            let id = auth_id(
                declaration.kind.as_deref().unwrap_or_default(),
                account.as_deref(),
            );
            auths.entry(id).or_insert(provider);
        }
        for (id, provider) in &auths {
            auth.register_as(id.clone(), Arc::clone(provider));
        }

        let vendors = Vendors {
            credentials: Arc::clone(&credentials),
            auths,
            catalog: catalog.clone(),
        };

        // Declarations first, so `[providers.grok]` *configures* the built-in
        // rather than colliding with it. A person who names a route has said
        // more about it than a default can, and the default that would have
        // taken the name is the one that yields.
        for declaration in declared {
            let provider = vendors.instance(declaration)?;
            providers.register(provider).with_context(|| {
                format!("registering declared provider `{}`", declaration.route)
            })?;
        }

        for default in all.iter().skip(declared.len()) {
            let route = default.route.clone();
            let provider = vendors.instance(default)?;
            providers
                .register(provider)
                .with_context(|| format!("registering the {route} provider"))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn vendors(home: &std::path::Path) -> Vendors {
        let credentials: Arc<dyn CredentialStore> =
            Arc::new(keke_credentials::MemoryStore::default());
        let auth_files = keke_credentials::VendorAuthStore::new(
            keke_paths::AbsPath::new(home).expect("an absolute test home"),
        );
        Vendors {
            auths: [
                (
                    "grok".to_string(),
                    Arc::new(keke_auth_grok::GrokAuth::with_defaults(
                        Arc::clone(&credentials),
                        auth_files.clone(),
                    )) as Arc<dyn AuthProvider>,
                ),
                (
                    "codex".to_string(),
                    Arc::new(keke_auth_codex::CodexAuth::with_defaults(
                        Arc::clone(&credentials),
                        auth_files,
                    )) as Arc<dyn AuthProvider>,
                ),
            ]
            .into_iter()
            .collect(),
            catalog: keke_catalog::CatalogCache::new(
                &keke_paths::AbsPath::new(home).expect("an absolute test home"),
                std::time::Duration::from_secs(60),
            ),
            credentials,
        }
    }

    fn declaration(route: &str, kind: &str) -> keke_config_types::ProviderDeclaration {
        builtin(route, kind)
    }

    /// The whole point of naming instances: one vendor, two addresses, two
    /// routes, neither the other's special case.
    #[test]
    fn one_vendor_can_serve_two_routes_at_two_addresses() {
        let home = tempfile::tempdir().expect("a temp home");
        let vendors = vendors(home.path());

        let mut subscription = declaration("grok", "grok");
        subscription.base_url = Some(GROK_SUBSCRIPTION_BASE_URL.to_string());
        subscription.wire = Some(keke_config_types::DeclaredWireApi::Responses);

        let mut pay_per_token = declaration("xai", "grok");
        pay_per_token.base_url = Some(keke_provider_grok::DEFAULT_BASE_URL.to_string());
        pay_per_token.wire = Some(keke_config_types::DeclaredWireApi::ChatCompletions);

        let mut providers = ProviderRegistry::new();
        providers
            .register(vendors.instance(&subscription).expect("the grok instance"))
            .expect("registering grok");
        providers
            .register(vendors.instance(&pay_per_token).expect("the xai instance"))
            .expect("registering xai");

        let grok = providers.get("grok").expect("grok is registered");
        let xai = providers.get("xai").expect("xai is registered");
        assert_eq!(grok.info().base_url, GROK_SUBSCRIPTION_BASE_URL);
        assert_eq!(xai.info().base_url, keke_provider_grok::DEFAULT_BASE_URL);
        assert_eq!(grok.info().wire_api, WireApi::Responses);
        assert_eq!(xai.info().wire_api, WireApi::ChatCompletions);
    }

    /// A stated address is the instance's own. The credential decided this
    /// before, which is how a login nobody could refresh pinned every session
    /// to the subscription proxy.
    #[test]
    fn a_stated_address_is_not_overridden_by_the_stored_credential() {
        let home = tempfile::tempdir().expect("a temp home");
        let vendors = vendors(home.path());

        let mut declared = declaration("xai", "grok");
        declared.base_url = Some("https://api.x.ai/v1".to_string());
        declared.wire = Some(keke_config_types::DeclaredWireApi::ChatCompletions);

        let provider = vendors.instance(&declared).expect("the xai instance");
        assert_eq!(provider.info().base_url, "https://api.x.ai/v1");
        assert_eq!(provider.info().wire_api, WireApi::ChatCompletions);
    }

    /// Two instances of one vendor on two accounts must draw on two
    /// credentials. Sharing one would have every surface report on an account
    /// no session was using.
    #[test]
    fn two_accounts_of_one_vendor_get_two_auth_ids() {
        let home = tempfile::tempdir().expect("a temp home");
        let credentials: Arc<dyn CredentialStore> =
            Arc::new(keke_credentials::MemoryStore::default());
        let auth_files = keke_credentials::VendorAuthStore::new(
            keke_paths::AbsPath::new(home.path()).expect("an absolute test home"),
        );
        let account = |name: &str| -> Arc<dyn AuthProvider> {
            Arc::new(
                keke_auth_grok::GrokAuth::with_defaults(
                    Arc::clone(&credentials),
                    auth_files.clone(),
                )
                .as_account(Some(name.to_string())),
            )
        };
        let vendors = Vendors {
            auths: [
                ("grok".to_string(), account("me@home.com")),
                ("grok/work@corp.com".to_string(), account("work@corp.com")),
            ]
            .into_iter()
            .collect(),
            credentials,
            catalog: keke_catalog::CatalogCache::new(
                &keke_paths::AbsPath::new(home.path()).expect("an absolute test home"),
                std::time::Duration::from_secs(60),
            ),
        };

        let mut work = declaration("grok-work", "grok");
        work.account = Some("work@corp.com".to_string());

        let personal = vendors
            .instance(&declaration("grok", "grok"))
            .expect("grok");
        let work = vendors.instance(&work).expect("grok-work");
        assert_eq!(personal.info().auth_id.as_deref(), Some("grok"));
        assert_eq!(work.info().auth_id.as_deref(), Some("grok/work@corp.com"));
    }

    /// Invariant 8: a kind nobody implements is an error, not a silent fall
    /// back to the generic wire provider.
    #[test]
    fn an_unknown_kind_is_refused_by_name() {
        let home = tempfile::tempdir().expect("a temp home");
        let vendors = vendors(home.path());
        let error = match vendors.instance(&declaration("mystery", "not-a-vendor")) {
            Err(error) => error,
            Ok(_) => panic!("an unknown kind must not resolve"),
        };
        let text = error.to_string();
        assert!(text.contains("not-a-vendor"), "{text}");
        assert!(text.contains("grok"), "the known kinds are listed: {text}");
    }

    /// A declaration with neither an address nor a kind to inherit one from
    /// would register a route that resolves and never connects.
    #[test]
    fn a_declaration_with_no_address_is_refused() {
        let home = tempfile::tempdir().expect("a temp home");
        let vendors = vendors(home.path());
        let mut declared = declaration("nowhere", "grok");
        declared.kind = None;
        let error = match vendors.instance(&declared) {
            Err(error) => error,
            Ok(_) => panic!("a declaration with no address must not resolve"),
        };
        assert!(error.to_string().contains("base_url"), "{error}");
    }
}
