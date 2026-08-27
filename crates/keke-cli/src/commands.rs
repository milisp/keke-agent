//! Command implementations.

use std::io::Read;
use std::io::Write;
use std::sync::Arc;

use anyhow::Context as _;
use anyhow::Result;
use anyhow::bail;
use keke_auth_api::AuthProvider;
use keke_auth_api::CredentialRef;
use keke_config::Config;
use keke_config_types::HomeLayout;
use keke_config_types::ModelSelection;
use keke_core::SessionBuilder;
use keke_core::SessionConfig;
use keke_core::TurnUpdate;
use keke_paths::AbsPath;
use keke_protocol::Message;
use keke_protocol::StopReason;

use crate::api_key::ApiKeyAuth;
use crate::cli::Cli;
use crate::cli::Command;
use crate::cli::ExecArgs;
use crate::cli::ExecFormat;
use crate::cli::LoginArgs;
use crate::cli::PluginAction;
use crate::cli::ResumeArgs;
use crate::cli::VendorArgs;
use crate::compose::Composed;
use crate::ui::TerminalLoginUi;
use crate::ui::is_interactive;

pub(crate) async fn run(cli: Cli) -> Result<()> {
    let cwd = match &cli.cwd {
        Some(path) => path.clone(),
        None => std::env::current_dir().context("reading the current directory")?,
    };
    let workspace_root = keke_config::resolve_workspace_root(&cwd)?;
    let mut config = Config::load(workspace_root.as_path())?;

    // CLI flags win over every config layer, which is what makes a one-off
    // override possible without editing a file. Resuming needs to tell a flag
    // typed for this run apart from the config default, so it knows not to
    // clobber a flag with what the session logged.
    let model_explicit = cli.model.is_some();
    let provider_explicit = cli.provider.is_some();
    let effort_explicit = cli.reasoning_effort.is_some();
    if let Some(provider) = cli.provider {
        // A `model` left over from config.toml names a model on whatever route
        // was last used, not this one — sending it to a different provider is
        // a combination no run ever chose. Dropping it lets `session_builder`
        // fall through to the new route's declared `default_model`, or its
        // model list, the same as a bare `keke --provider <x>` with no prior
        // config should behave.
        if provider != config.model.provider {
            config.model.model.clear();
        }
        config.model.provider = provider;
    }
    if let Some(model) = cli.model {
        config.model.model = model;
    }
    if let Some(effort) = cli.reasoning_effort {
        config.reasoning_effort = Some(effort);
    }

    // Only the interactive surface can answer an approval request, so only it
    // installs the bridge; everything else runs with the engine's default.
    let command = cli.command.unwrap_or(Command::Tui);
    // `resume` is the interface too, so it installs the approval bridge for the
    // same reason: it is the one surface with somebody to ask.
    let interactive = matches!(command, Command::Tui | Command::Resume(_));
    let (approvals, requests) = keke_acp::approvals();
    let composed = Composed::build(
        &config.home,
        &config.providers,
        config.plugins,
        config.model_catalog_ttl,
        config.subagents,
        interactive.then(|| Arc::clone(&approvals)),
    )?;

    // A directory override that names a route nobody registered is not a
    // preference that can be ignored: silently falling back would run the turn
    // on whatever account the global default names, which for a work checkout
    // is the wrong one and says so nowhere.
    // A `--provider` on the command line has already replaced whatever the
    // override chose, so there is nothing left of it to check.
    if let Some(applied) = &config.directory_override
        && let Some(route) = applied.provider.as_deref().filter(|_| !provider_explicit)
        && composed.providers.get(route).is_err()
    {
        bail!(
            "the directory override for match = \"{}\" names provider `{route}`, which is not configured; known routes: {}",
            applied.pattern,
            composed.providers.routes().collect::<Vec<_>>().join(", ")
        );
    }

    // A first run that declares an endpoint changes what the registry should
    // contain, and the registry is frozen once built — so it is rebuilt rather
    // than mutated, and the person's own answer works in the session they gave
    // it in rather than after a restart.
    let mut composed = composed;
    if matches!(command, Command::Tui) && is_interactive() {
        match crate::first_run::maybe_run(&mut config, &composed).await? {
            crate::first_run::Outcome::ProvidersChanged => {
                composed = Composed::build(
                    &config.home,
                    &config.providers,
                    config.plugins,
                    config.model_catalog_ttl,
                    config.subagents,
                    interactive.then(|| Arc::clone(&approvals)),
                )?;
            }
            // Not an error: the person was asked a question and chose not to
            // answer it. Exiting quietly leaves them where they can act on
            // what the picker just told them.
            crate::first_run::Outcome::Abandoned => return Ok(()),
            crate::first_run::Outcome::Unchanged => {}
        }
    }

    match command {
        Command::Tui => {
            tui(
                config,
                composed,
                cwd,
                approvals,
                requests,
                keke_tui::Resumed::default(),
                None,
            )
            .await
        }
        Command::Resume(args) => {
            resume(
                args,
                config,
                composed,
                cwd,
                approvals,
                requests,
                model_explicit,
                effort_explicit,
            )
            .await
        }
        Command::Exec(args) => exec(args, config, composed, cwd).await,
        Command::Agent { transport } => agent(transport, config, cwd).await,
        Command::Login(args) => login(args, composed).await,
        Command::Logout(args) => logout(args, composed).await,
        Command::Models(args) => models(args, composed).await,
        Command::Doctor => doctor(config, composed),
        Command::Plugin { action } => plugin(action, config),
    }
}

/// What `route` serves, or nothing when it could not be asked.
///
/// A compiled-in vendor answers from its own catalog when the network does
/// not, so "nothing" here means the route does not exist or genuinely
/// publishes no list — not that a request failed.
async fn models_for(composed: &Composed, route: &str) -> Vec<keke_provider_api::ModelInfo> {
    let Ok(provider) = provider_for(composed, route) else {
        return Vec::new();
    };
    match provider.list_models().await {
        Ok(models) => models,
        Err(error) => {
            tracing::warn!(%route, %error, "could not list this provider's models");
            Vec::new()
        }
    }
}

/// Every provider instance a person could point the next session at, as
/// `/provider` needs to describe it.
fn provider_choices(composed: &Composed) -> Vec<keke_tui::ProviderChoice> {
    let routes: Vec<String> = composed.providers.routes().map(str::to_string).collect();
    routes
        .into_iter()
        .filter_map(|route| {
            let provider = composed.providers.get(&route).ok()?;
            Some(keke_tui::ProviderChoice {
                display_name: provider.info().display_name.clone(),
                route,
            })
        })
        .collect()
}

/// The command list a person completes against, wherever the interface is.
///
/// Built once here so the TUI's own completion and the list an ACP client is
/// told about (`EditorSessions::start`) always agree — a name only ever gets
/// resolved once, not separately by two callers that could disagree about
/// which plugin won a collision.
fn slash_commands(composed: &Composed) -> keke_tui::SlashCommands {
    let commands = composed
        .commands
        .iter()
        .map(|command| {
            keke_tui::SlashCommand::prompt(
                &command.plugin,
                &command.name,
                &command.description,
                command.path.as_path(),
            )
        })
        .collect();
    keke_tui::SlashCommands::new(commands)
}

/// The plugin-contributed subset of `commands`, for an ACP client's own
/// autocomplete.
///
/// Builtins (`/quit`, `/copy`, ...) are TUI behavior with no ACP equivalent —
/// an editor has its own UI for clearing a transcript — so only what a plugin
/// actually contributed is advertised.
fn plugin_commands(commands: &keke_tui::SlashCommands) -> Vec<keke_acp::PluginCommand> {
    commands
        .entries()
        .iter()
        .filter_map(|entry| match &entry.action {
            keke_tui::slash::SlashAction::Prompt(_) => Some(keke_acp::PluginCommand {
                name: entry.name.clone(),
                description: entry.description.clone(),
            }),
            keke_tui::slash::SlashAction::Builtin(_) => None,
        })
        .collect()
}

/// Resolve the provider for `route`, failing with what is actually available.
fn provider_for(composed: &Composed, route: &str) -> Result<keke_provider_api::ArcProvider> {
    // `context` would make the hint the headline and the failure the cause,
    // which reads backwards; the failure is what happened.
    composed.providers.get(route).map_err(|error| {
        anyhow::anyhow!(
            "{error}\n\navailable providers: {}",
            composed.providers.routes().collect::<Vec<_>>().join(", ")
        )
    })
}

/// The env key a route needs but has no usable credential for, when it has
/// no login flow to ask `auth_for` about instead.
///
/// `None` covers two different truths callers must not conflate: the route
/// takes no credential at all (a local server), or it has one and the
/// credential store can resolve it. Either way there is nothing to report.
fn unusable_key(composed: &Composed, info: &keke_provider_api::ProviderInfo) -> Option<String> {
    let env_key = info.env_key.as_deref()?;
    let reference = CredentialRef::new(env_key).ok()?;
    let resolved = composed
        .credentials
        .load(&reference)
        .ok()
        .flatten()
        .is_some();
    (!resolved).then(|| env_key.to_string())
}

/// Assemble the session every surface runs on.
///
/// Shared so the interface and `exec` cannot drift into offering different
/// tools, a different budget, or a different approval policy.
async fn session_builder(
    config: &Config,
    composed: &Composed,
    cwd: std::path::PathBuf,
    approval: keke_config_types::ApprovalPolicy,
) -> Result<SessionBuilder> {
    let route = config.model.provider.clone();
    let provider = provider_for(composed, &route)?;
    let auth = composed.auth_for(&route);

    // Checked before the turn starts: discovering this after a rollout log has
    // been opened and a request built is a worse experience than one line here.
    //
    // `auth_for` only answers for a route with a registered login flow
    // (`ProviderInfo::auth_id`); a route backed by a plain API key — like
    // `anthropic`, or any declared endpoint with `env_key` set — registers no
    // entry there, so `auth` is `None` for it and this `if` alone would let a
    // session open with no credential at all, to fail only once a request
    // goes out. `unusable_key` covers that second path the same way
    // `first_run::unusable` does for the interactive picker.
    if auth
        .as_ref()
        .is_some_and(|auth| !auth.has_usable_credential())
    {
        bail!("not signed in to `{route}`; run `keke login {route}`");
    }
    if auth.is_none()
        && let Some(env_key) = unusable_key(composed, provider.info())
    {
        bail!("no {env_key} stored for `{route}`; export it, or set it in the client's login");
    }

    // Nothing chose a model — no flag, no config layer — so the provider is
    // asked what it serves rather than a constant being guessed at. A vendor
    // that publishes no list leaves nothing to fall back on, and saying so
    // here beats sending an empty model id and reading the vendor's rejection.
    // Before asking the vendor, honor what this deployment already decided:
    // a declared provider's `default_model` is the person's own answer to "what
    // should this route serve", so it beats whatever heads the model list.
    let model = match config.model.model.trim() {
        "" => match declared_default_model(config, &route) {
            Some(model) => model,
            None => match provider.list_models().await {
                Ok(models) if !models.is_empty() => models[0].id.clone(),
                Ok(_) => bail!(
                    "no model set and `{route}` publishes no model list — set `model` in config.toml or pass --model"
                ),
                Err(error) => {
                    bail!("no model set and `{route}` could not be asked for one: {error}")
                }
            },
        },
        chosen => chosen.to_string(),
    };

    let mut builder = SessionBuilder::new()
        .config(SessionConfig {
            model: ModelSelection {
                provider: route,
                model,
            },
            home: HomeLayout {
                home: config.home.home.clone(),
                workspace_root: config.home.workspace_root.clone(),
            },
            max_output_tokens: config.max_output_tokens,
            reasoning_effort: config.reasoning_effort,
            compaction: config.compaction,
            approval,
        })
        .provider(provider)
        .extensions(composed.extensions.clone())
        .cwd(cwd);

    if let Some(auth) = auth {
        builder = builder.auth(auth);
    }

    // Handed over here rather than in `Composed::build`: the recipe a subagent
    // is built from *is* a session builder, and there is no finished builder
    // until this function has one. Deliberately before any surface attaches
    // live updates — a subagent streams to nobody and reports once, at the end.
    composed.subagents.attach(builder.clone());

    Ok(builder)
}

/// Serve ACP to an editor.
///
/// The editor is the one asking a person, so approval requests travel to it
/// over the protocol rather than being answered here.
async fn agent(
    transport: crate::cli::AgentTransport,
    config: Config,
    cwd: std::path::PathBuf,
) -> Result<()> {
    let crate::cli::AgentTransport::Stdio = transport;
    let factory = Arc::new(EditorSessions {
        config,
        cwd,
        route: std::sync::Mutex::new(None),
    });
    keke_acp::serve_stdio(factory)
        .await
        .map_err(|error| anyhow::anyhow!("the ACP connection failed: {error}"))
}

/// Opens one keke session per ACP session.
///
/// The vendors are composed per session rather than once, because the approval
/// bridge is part of the frozen extension set and two sessions sharing one
/// would route a prompt raised in one to whoever answered in the other.
struct EditorSessions {
    config: Config,
    cwd: std::path::PathBuf,
    /// The route `authenticate` last signed the client in to, overriding
    /// `config.model.provider` for every session opened afterward on this
    /// connection.
    ///
    /// Behind a lock because `authenticate` and `session/new` are separate
    /// RPCs on the same connection: the client authenticates once, then opens
    /// however many sessions follow, and each must see the same answer.
    route: std::sync::Mutex<Option<String>>,
}

impl EditorSessions {
    /// The directory to root a session in.
    ///
    /// The client names it; keke's own `--cwd` is the fallback for a client
    /// that does not.
    fn rooted_at(&self, cwd: std::path::PathBuf) -> std::path::PathBuf {
        if cwd.as_os_str().is_empty() {
            self.cwd.clone()
        } else {
            cwd
        }
    }

    /// The route to open a session against: what `authenticate` last chose,
    /// or the server's own configured default.
    fn active_route(&self) -> String {
        self.route
            .lock()
            .ok()
            .and_then(|route| route.clone())
            .unwrap_or_else(|| self.config.model.provider.clone())
    }

    /// Write the chosen route to `$KEKE_HOME/config.toml`, the way the TUI
    /// persists `/model`.
    ///
    /// Without this, signing in over ACP lasts exactly as long as the
    /// connection: a client that reconnects — for a new working directory, or
    /// because the person reloaded a page — lands back on the configured
    /// default route and is told to sign in to a vendor it already has
    /// credentials for.
    ///
    /// The model travels with the route. A model name belongs to one vendor,
    /// so carrying the old one over would persist a pairing that cannot serve
    /// a single request; the route's own first model is a wrong-but-working
    /// starting point the client can then change, and the effort goes with it
    /// because a ladder is a property of the model that named it.
    ///
    /// Best-effort throughout: a home that cannot be written is a worse
    /// session next time, not a failed login now.
    async fn remember_route(&self, composed: &Composed, route: &str) {
        let models = models_for(composed, route).await;
        let replacement = (!models
            .iter()
            .any(|model| model.id == self.config.model.model))
        .then(|| models.first().map(|model| model.id.clone()))
        .flatten();
        if let Err(error) = keke_config::persist_user_override(&self.config.home.home, |file| {
            file.provider = Some(route.to_string());
            if let Some(model) = replacement {
                file.model = Some(model);
                file.reasoning_effort = None;
            }
        }) {
            tracing::warn!(%route, %error, "could not persist the route to config.toml");
        }
    }

    /// What the session's provider serves, for the client to choose between.
    ///
    /// A provider that cannot be asked leaves the list empty rather than
    /// failing the session: not being able to switch models is a smaller loss
    /// than not being able to open the conversation at all.
    async fn models(&self, composed: &Composed, route: &str) -> Vec<keke_provider_api::ModelInfo> {
        models_for(composed, route).await
    }

    /// Build one session, new or continuing, and start it.
    async fn start(
        &self,
        cwd: std::path::PathBuf,
        resume: Option<keke_core::ResumedSession>,
    ) -> Result<keke_acp::Opened> {
        let (approvals, requests) = keke_acp::approvals();
        let composed = Composed::build(
            &self.config.home,
            &self.config.providers,
            self.config.plugins,
            self.config.model_catalog_ttl,
            self.config.subagents,
            Some(Arc::clone(&approvals)),
        )?;
        // What the session was last talking to wins over the server's config
        // default: a client reopening a session means to continue it, not to
        // switch it back to whatever keke was started with.
        let mut config = self.config.clone();
        config.model.provider = self.active_route();
        if let Some(model) = resume.as_ref().and_then(|resumed| resumed.model.clone()) {
            config.model.model = model;
        }
        if let Some(effort) = resume.as_ref().and_then(|resumed| resumed.reasoning_effort) {
            config.reasoning_effort = Some(effort);
        }
        if let Some(policy) = resume.as_ref().and_then(|resumed| resumed.approval_policy) {
            config.approval_policy = policy;
        }
        let mut builder = session_builder(&config, &composed, cwd, config.approval_policy).await?;
        let history = resume.as_ref().map(|resumed| resumed.history.clone());
        if let Some(resumed) = resume {
            builder = builder.resume(resumed.id, resumed.history);
        }
        let mut opened = keke_acp::local(builder, approvals, requests).await?;
        opened.history = history.unwrap_or_default();
        opened.models = self.models(&composed, &config.model.provider).await;
        // The same resolved list the TUI would complete against, so an ACP
        // editor's own autocomplete offers exactly what keke's own interface
        // would — see `slash_commands`.
        opened.commands = plugin_commands(&slash_commands(&composed));
        Ok(opened)
    }
}

impl keke_acp::SessionFactory for EditorSessions {
    fn open(
        &self,
        cwd: std::path::PathBuf,
    ) -> keke_acp::ConversationFuture<'_, Result<keke_acp::Opened, keke_acp::ConversationError>>
    {
        Box::pin(async move {
            self.start(self.rooted_at(cwd), None)
                .await
                .map_err(|error| keke_acp::ConversationError::Agent(error.to_string()))
        })
    }

    /// Every route there is to authenticate to, not only the ones with a
    /// login flow — a route wanting an API key, or wanting nothing at all
    /// (a local server), is just as much an answer to "how do I sign in?" as
    /// one with OAuth behind it. Mirrors the three-way split
    /// `first_run::pick` offers interactively, minus the local-server presets
    /// that only exist once someone has declared them.
    fn auth_methods(&self) -> Vec<keke_acp::AuthMethodDescriptor> {
        let Ok(composed) = Composed::build(
            &self.config.home,
            &self.config.providers,
            self.config.plugins,
            self.config.model_catalog_ttl,
            self.config.subagents,
            None,
        ) else {
            return Vec::new();
        };
        composed
            .providers
            .routes()
            .filter_map(|route| {
                let handle = composed.providers.get(route).ok()?;
                let info = handle.info();
                // The same three-way split the description makes, answered
                // for the credential that is actually there: a client showing
                // a sign-in menu has to be able to tell "sign in" from
                // "already signed in, through this".
                let (signed_in, source, description) = match composed.auth_for(route) {
                    Some(auth) if auth.has_usable_credential() => {
                        let source = auth.snapshot().source;
                        (
                            true,
                            Some(source.clone()),
                            format!("Signed in with {} ({source})", info.display_name),
                        )
                    }
                    Some(_) => (false, None, format!("Sign in with {}", info.display_name)),
                    None => match info.env_key.as_deref() {
                        Some(env_key) => {
                            let stored = unusable_key(&composed, info).is_none();
                            (
                                stored,
                                stored.then(|| env_key.to_string()),
                                if stored {
                                    format!("API key ({env_key}) is set")
                                } else {
                                    format!("API key ({env_key})")
                                },
                            )
                        }
                        // Takes no credential at all (a local server): usable
                        // by definition, so it is signed in by definition.
                        None => (true, None, "No credential needed".to_string()),
                    },
                };
                Some(keke_acp::AuthMethodDescriptor {
                    id: route.to_string(),
                    name: info.display_name.clone(),
                    description: Some(description),
                    signed_in,
                    source,
                })
            })
            .collect()
    }

    /// Settle one route's credential, then make it the route new sessions
    /// open against — on this connection, and on the next one.
    ///
    /// `meta.apiKey` lets a client that collected a key itself hand it
    /// straight over — the same act as `keke login`'s key prompt, just spoken
    /// over ACP instead of a terminal — but is never required: a route whose
    /// credential is already resolvable (stored, or in the environment) or
    /// that needs none at all succeeds without it.
    ///
    /// `meta.force` re-runs the login flow even then. A stored credential is
    /// only ever known to be *present*, never to be *good* — an expired
    /// token, or a key some other tool left behind — so a person who asks to
    /// sign in again must get a login rather than a silent success that
    /// leaves the bad credential in place.
    fn authenticate(
        &self,
        method_id: &str,
        meta: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> keke_acp::ConversationFuture<'_, Result<(), keke_acp::ConversationError>> {
        let method_id = method_id.to_string();
        let pasted_key = meta
            .as_ref()
            .and_then(|meta| meta.get("apiKey"))
            .and_then(|value| value.as_str())
            .map(str::to_string);
        let force = meta
            .as_ref()
            .and_then(|meta| meta.get("force"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        Box::pin(async move {
            let composed = Composed::build(
                &self.config.home,
                &self.config.providers,
                self.config.plugins,
                self.config.model_catalog_ttl,
                self.config.subagents,
                None,
            )
            .map_err(|error| keke_acp::ConversationError::Agent(error.to_string()))?;
            let provider = composed.providers.get(&method_id).map_err(|_| {
                keke_acp::ConversationError::Agent(format!(
                    "unknown authentication method `{method_id}`"
                ))
            })?;

            if let Some(auth) = composed.auth_for(&method_id) {
                if force || !auth.has_usable_credential() {
                    auth.login(Arc::new(crate::ui::AcpLoginUi))
                        .await
                        .map_err(|error| keke_acp::ConversationError::Agent(error.to_string()))?;
                }
            } else if let Some(env_key) = provider.info().env_key.clone() {
                let reference = CredentialRef::new(env_key.clone())
                    .map_err(|error| keke_acp::ConversationError::Agent(error.to_string()))?;
                let already_usable = composed
                    .credentials
                    .load(&reference)
                    .ok()
                    .flatten()
                    .is_some();
                if let Some(key) = pasted_key {
                    composed
                        .credentials
                        .save(&reference, &key)
                        .map_err(|error| keke_acp::ConversationError::Agent(error.to_string()))?;
                } else if force || !already_usable {
                    let stored = if already_usable {
                        format!("the stored {env_key} was not replaced")
                    } else {
                        format!("no {env_key} stored")
                    };
                    return Err(keke_acp::ConversationError::Agent(format!(
                        "{stored}; pass one in `authenticate`'s `apiKey` field, \
                         or export {env_key} and try again"
                    )));
                }
            }
            // Neither branch: the route takes no credential (a local server),
            // and is usable by definition.

            if let Ok(mut route) = self.route.lock() {
                *route = Some(method_id.clone());
            }
            self.remember_route(&composed, &method_id).await;
            Ok(())
        })
    }

    fn list(
        &self,
        cwd: Option<std::path::PathBuf>,
    ) -> keke_acp::ConversationFuture<
        '_,
        Result<Vec<keke_acp::SessionListing>, keke_acp::ConversationError>,
    > {
        Box::pin(async move {
            let sessions = keke_core::list_sessions(&self.config.home.home)
                .map_err(|error| keke_acp::ConversationError::Agent(error.to_string()))?;
            Ok(sessions
                .into_iter()
                // A log with no turns is an interface someone opened and
                // closed; listing them buries the conversations under the
                // empty files, exactly as `keke resume --list` decided.
                .filter(|session| session.turns > 0)
                .filter(|session| match (&cwd, &session.cwd) {
                    (Some(wanted), Some(started_in)) => std::path::Path::new(started_in) == wanted,
                    // A filter the log cannot answer excludes the session
                    // rather than guessing it matches.
                    (Some(_), None) => false,
                    (None, _) => true,
                })
                .map(|session| keke_acp::SessionListing {
                    id: session.id.to_string(),
                    cwd: session
                        .cwd
                        .as_ref()
                        .map_or_else(|| self.cwd.clone(), std::path::PathBuf::from),
                    title: session.summary,
                    updated_at: session.updated_at,
                })
                .collect())
        })
    }

    fn resume(
        &self,
        id: String,
        cwd: std::path::PathBuf,
    ) -> keke_acp::ConversationFuture<'_, Result<keke_acp::Opened, keke_acp::ConversationError>>
    {
        Box::pin(async move {
            self.reopen(id, cwd)
                .await
                .map_err(|error| keke_acp::ConversationError::Agent(error.to_string()))
        })
    }
}

impl EditorSessions {
    /// Resolve what the client sent back to one session, and continue it.
    async fn reopen(&self, id: String, cwd: std::path::PathBuf) -> Result<keke_acp::Opened> {
        let home = &self.config.home.home;
        // The same prefix matching `keke resume` takes, so a client may hand
        // back either the id it was shown or the whole thing.
        let session = match keke_core::find_session(home, &id)? {
            keke_core::SessionMatch::One(session) => session,
            // Invariant 8: two claimants and no way to choose is an error, not
            // a pick.
            keke_core::SessionMatch::Ambiguous(candidates) => {
                bail!("`{id}` matches {} sessions", candidates.len())
            }
            keke_core::SessionMatch::None => bail!("no session `{id}`"),
        };
        let resumed = keke_core::load_session(home, session.id)
            .with_context(|| format!("reading the log for session {}", session.id))?;
        // Where the session was started wins over where the client says it is:
        // pointing a resumed conversation's tools at another directory is a
        // different session wearing the same name.
        let cwd = resumed
            .cwd
            .as_ref()
            .map_or_else(|| self.rooted_at(cwd), std::path::PathBuf::from);
        self.start(cwd, Some(resumed)).await
    }
}

/// Reopen a previous session, or list what there is to reopen.
///
/// The history comes from the rollout log and nowhere else: what keke can
/// replay is what keke can continue, so there is no second record for the two
/// to disagree about.
async fn resume(
    args: ResumeArgs,
    mut config: Config,
    composed: Composed,
    cwd: std::path::PathBuf,
    approvals: Arc<keke_acp::Approvals>,
    requests: keke_acp::ApprovalRequests,
    model_explicit: bool,
    effort_explicit: bool,
) -> Result<()> {
    let home = &config.home.home;
    let sessions = keke_core::list_sessions(home)?;
    // A log with no turns is an interface someone opened and closed; listing
    // them buries the conversations under the empty files.
    let conversations: Vec<_> = sessions
        .iter()
        .filter(|session| args.all || session.turns > 0)
        .collect();

    if args.list {
        if conversations.is_empty() {
            println!(
                "no sessions under {}",
                keke_core::sessions_dir(home).display()
            );
            return Ok(());
        }
        println!(
            "{:<10} {:<20} {:>5}  STARTED WITH",
            "ID", "UPDATED", "TURNS"
        );
        for session in conversations {
            println!(
                "{:<10} {:<20} {:>5}  {}",
                session.short_id(),
                session.updated_at.get(..19).unwrap_or("-"),
                session.turns,
                session.summary
            );
        }
        println!("\nresume one with `keke resume <id>`, or the last one with `keke resume`");
        return Ok(());
    }

    let id = match &args.session {
        // Any prefix of an id, because nobody retypes a UUID: `--list` prints
        // the short form and this takes it back.
        Some(typed) => match keke_core::find_session(home, typed)? {
            keke_core::SessionMatch::One(session) => session.id,
            // Invariant 8: two claimants and no way to choose is an error, not
            // a pick — continuing the wrong conversation is silent and costly.
            keke_core::SessionMatch::Ambiguous(candidates) => {
                let named = candidates
                    .iter()
                    .map(|session| format!("  {}  {}", session.short_id(), session.summary))
                    .collect::<Vec<_>>()
                    .join("\n");
                bail!("`{typed}` matches {} sessions:\n{named}", candidates.len());
            }
            keke_core::SessionMatch::None => {
                bail!("no session starts with `{typed}`; `keke resume --list` shows what there is");
            }
        },
        None => {
            keke_core::latest_session(home)?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "no session to resume under {}",
                        keke_core::sessions_dir(home).display()
                    )
                })?
                .id
        }
    };

    let resumed = keke_core::load_session(home, id)
        .with_context(|| format!("reading the log for session {id}"))?;
    // Where the session was started wins over where keke was invoked: resuming
    // a conversation about another directory and silently pointing its tools at
    // this one would be a different session wearing the same name.
    let cwd = resumed.cwd.as_ref().map_or(cwd, std::path::PathBuf::from);
    // What the session was last talking to wins over the config default — a
    // flag typed for this run still wins over that, since it is the more
    // specific instruction.
    if !model_explicit && let Some(model) = &resumed.model {
        config.model.model = model.clone();
    }
    if !effort_explicit && resumed.reasoning_effort.is_some() {
        config.reasoning_effort = resumed.reasoning_effort;
    }
    // No flag overrides this one yet, so what the session was last set to
    // always wins over the config default.
    if let Some(policy) = resumed.approval_policy {
        config.approval_policy = policy;
    }
    let seed = keke_tui::Resumed {
        history: resumed.history.clone(),
        usage: resumed.usage,
        context_input: resumed.context_input,
    };
    tui(
        config,
        composed,
        cwd,
        approvals,
        requests,
        seed,
        Some((id, resumed.history)),
    )
    .await
}

/// Open the interactive interface.
async fn tui(
    config: Config,
    composed: Composed,
    cwd: std::path::PathBuf,
    approvals: Arc<keke_acp::Approvals>,
    requests: keke_acp::ApprovalRequests,
    seed: keke_tui::Resumed,
    resume: Option<(keke_protocol::SessionId, Vec<keke_protocol::Message>)>,
) -> Result<()> {
    // The directory the typing history belongs to: for a resumed session, the
    // one it was started in rather than wherever keke was invoked.
    let history_cwd = cwd.clone();
    let mut builder = session_builder(&config, &composed, cwd, config.approval_policy).await?;
    if let Some((id, history)) = resume {
        builder = builder.resume(id, history);
    }
    let commands = slash_commands(&composed);
    // Asked before the interface opens so `/model` can answer without a round
    // trip mid-conversation. It costs at most one request, and usually none:
    // the compiled-in vendors cache what they serve between runs.
    let models = models_for(&composed, &config.model.provider).await;
    let opened = keke_acp::local_with(
        builder,
        approvals,
        requests,
        Some(subagent_views(&composed.subagents)),
    )
    .await?;
    // Read only once the session has an id: a fresh session's id is minted
    // inside `session_builder`/`local`, and every recorded prompt should carry
    // the session it was actually typed in, not none at all.
    let session_id = opened
        .id
        .parse::<uuid::Uuid>()
        .map(keke_protocol::SessionId::from)
        .ok();
    let prompts = prompt_history(&config.home.home, &history_cwd, session_id);
    let (conversation, updates) = (opened.conversation, opened.updates);
    keke_tui::run(
        conversation,
        updates,
        commands,
        keke_tui::SessionDefaults {
            approval: config.approval_policy,
            effort: config.reasoning_effort,
            config_home: config.home.home.clone(),
        },
        keke_tui::Models {
            provider: config.model.provider.clone(),
            current: opened.model,
            available: models,
            routes: provider_choices(&composed),
        },
        seed,
        prompts,
    )
    .await
}

/// This project's past prompts, plus the sink new ones are appended to.
///
/// A history that cannot be read is an empty one: somebody opening keke wants
/// their session, not a startup failure over a convenience file.
/// Relay the subagent host's live rows onto the surface's update stream.
///
/// The mapping lives here because the composition root is the only place that
/// can see both ends: `keke-acp` does not know the engine has subagents, and
/// `keke-subagent` must not know a surface exists.
fn subagent_views(
    host: &std::sync::Arc<keke_subagent::SubagentHost>,
) -> tokio::sync::mpsc::UnboundedReceiver<Vec<keke_acp::SubagentView>> {
    let mut rows = host.subscribe();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(rows) = rows.recv().await {
            let views = rows
                .into_iter()
                .map(|row| keke_acp::SubagentView {
                    id: row.id,
                    task: row.task,
                    status: row.status.map(|status| status.as_str().to_string()),
                    input_tokens: row.input_tokens,
                })
                .collect();
            if tx.send(views).is_err() {
                break;
            }
        }
    });
    rx
}

fn prompt_history(
    home: &AbsPath,
    cwd: &std::path::Path,
    session: Option<keke_protocol::SessionId>,
) -> keke_tui::PromptHistory {
    let mut log = keke_core::PromptHistory::new(home, cwd);
    if let Some(session) = session {
        log = log.in_session(session);
    }
    let entries = log.load().unwrap_or_else(|error| {
        tracing::warn!(%error, "could not read the prompt history");
        Vec::new()
    });
    keke_tui::PromptHistory::new(entries).with_recorder(Arc::new(PromptLog(log)))
}

/// Appends what was typed to the project's history file.
struct PromptLog(keke_core::PromptHistory);

impl keke_tui::PromptRecorder for PromptLog {
    fn record(&self, prompt: &str) {
        // Losing a line of history is not worth failing the turn a person just
        // started, so this is reported and dropped.
        if let Err(error) = self.0.record(prompt) {
            tracing::warn!(%error, "could not record the prompt history");
        }
    }
}

async fn exec(
    args: ExecArgs,
    config: Config,
    composed: Composed,
    cwd: std::path::PathBuf,
) -> Result<()> {
    let prompt = match args.prompt {
        Some(prompt) => prompt,
        None => {
            let mut buffer = String::new();
            std::io::stdin()
                .read_to_string(&mut buffer)
                .context("reading the prompt from stdin")?;
            buffer
        }
    };
    if prompt.trim().is_empty() {
        bail!("no prompt given; pass one as an argument or on stdin");
    }

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let builder = session_builder(
        &config,
        &composed,
        cwd,
        args.approval.unwrap_or(config.approval_policy),
    )
    .await?
    .updates(tx);

    let mut session = builder.build().await?;
    let log_path = session.log_path().to_path_buf();

    // Ctrl-C cancels the turn rather than killing the process, so the rollout
    // log is closed cleanly and a partially written file is not left behind.
    let canceller = session.canceller();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("\ncancelling…");
            canceller();
        }
    });

    // JSON mode never streams partial text: it waits for the full reply and
    // emits one object. Text mode streams when connected to a terminal. Tool
    // call progress always goes to stderr so it stays visible in both modes.
    let streaming = args.format == ExecFormat::Text && is_interactive();
    let renderer = tokio::spawn(async move {
        let mut out = std::io::stdout();
        while let Some(update) = rx.recv().await {
            match update {
                TurnUpdate::TextDelta { delta, .. } if streaming => {
                    if write!(out, "{delta}").is_err() {
                        // Broken pipe: stop writing, do not panic.
                        break;
                    }
                    let _ = out.flush();
                }
                TurnUpdate::ToolCallStarted { call } => {
                    eprintln!("· {}", call.name);
                }
                _ => {}
            }
        }
    });

    let outcome = session.run_turn(Message::user(prompt)).await;
    drop(session);
    let _ = renderer.await;

    match args.format {
        ExecFormat::Text => {
            let outcome = outcome?;
            if !streaming {
                // Piped output gets the answer alone, with no interleaved progress.
                if let Some(message) = &outcome.message {
                    println!("{}", message.text());
                }
            } else {
                println!();
            }
            if args.print_log_path {
                eprintln!("log: {}", log_path.display());
            }
            match outcome.stop_reason {
                StopReason::Refusal { message } => bail!("the model refused: {message}"),
                StopReason::Cancelled => bail!("cancelled"),
                _ => Ok(()),
            }
        }
        ExecFormat::Json => {
            let outcome = match outcome {
                Ok(o) => o,
                Err(error) => {
                    // Surface the engine error as a JSON line so a script that
                    // parses stdout sees a consistent shape whether the turn
                    // succeeded or failed.
                    let obj = serde_json::json!({"type": "error", "message": error.to_string()});
                    emit_json(&obj);
                    return Err(error.into());
                }
            };

            let mut obj = serde_json::json!({
                "text": outcome.message.as_ref().map(|m| m.text()).unwrap_or_default(),
                "stopReason": stop_reason_wire(&outcome.stop_reason),
                "usage": {
                    "inputTokens": outcome.usage.input_tokens,
                    "outputTokens": outcome.usage.output_tokens,
                    "cachedInputTokens": outcome.usage.cached_input_tokens,
                    "reasoningTokens": outcome.usage.reasoning_tokens,
                },
            });
            if args.print_log_path {
                obj["log"] = serde_json::Value::String(log_path.display().to_string());
            }
            emit_json(&obj);

            match outcome.stop_reason {
                StopReason::Refusal { message } => bail!("the model refused: {message}"),
                StopReason::Cancelled => bail!("cancelled"),
                _ => Ok(()),
            }
        }
    }
}

/// Map a stop reason to its snake_case wire token.
///
/// Explicit mapping rather than serde so the token is a deliberate contract,
/// not an accident of how the field happens to be serialized right now. An
/// unknown future variant logs a warning and falls back to `end_turn` rather
/// than propagating a deserialization shape that callers have not seen.
fn stop_reason_wire(reason: &StopReason) -> &'static str {
    match reason {
        StopReason::EndTurn => "end_turn",
        StopReason::ToolUse => "tool_use",
        StopReason::MaxTokens => "max_tokens",
        StopReason::Cancelled => "cancelled",
        StopReason::Refusal { .. } => "refusal",
    }
}

/// Write a compact JSON object to stdout followed by a newline.
///
/// Broken pipe is treated as a clean stop: the caller already exited, and
/// panicking here would leave the rollout log open.
fn emit_json(value: &serde_json::Value) {
    use std::io::Write as _;
    let rendered = match serde_json::to_string_pretty(value) {
        Ok(s) => s,
        Err(_) => value.to_string(),
    };
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(rendered.as_bytes());
    let _ = out.write_all(b"\n");
}

async fn login(args: LoginArgs, composed: Composed) -> Result<()> {
    let auth = composed
        .auth
        .get(&args.vendor)
        .with_context(|| format!("unknown vendor `{}`", args.vendor))?;

    auth.login(Arc::new(TerminalLoginUi)).await?;

    let snapshot = auth.snapshot();
    println!("Signed in to {} via {}.", args.vendor, snapshot.source);
    if let Some(account) = snapshot.account_id {
        println!("  account: {account}");
    }
    Ok(())
}

async fn logout(args: VendorArgs, composed: Composed) -> Result<()> {
    let auth = composed
        .auth
        .get(&args.vendor)
        .with_context(|| format!("unknown vendor `{}`", args.vendor))?;
    auth.logout().await?;

    // An imported credential belongs to another tool's file, which keke will
    // not write. Reporting a clean sign-out while the next request still
    // authenticates would be a lie the user only discovers later.
    if auth.has_usable_credential() {
        let source = auth.snapshot().source;
        println!(
            "Removed keke's stored credentials for {}, but it is still signed in \n\
             through a credential keke does not own ({source}). keke reads the \n\
             codex and grok CLIs' logins and never writes to their files — sign \n\
             out with that tool to remove it.",
            args.vendor
        );
    } else {
        println!("Signed out of {}.", args.vendor);
    }
    Ok(())
}

async fn models(args: VendorArgs, composed: Composed) -> Result<()> {
    let provider = composed.providers.get(&args.vendor)?;
    let models = provider.list_models().await?;
    if models.is_empty() {
        println!("{} does not publish a model list.", args.vendor);
        return Ok(());
    }
    for model in models {
        println!("{}", model.id);
        // Indented under the id rather than beside it: the id is what gets
        // typed and copied, and a line that starts with it stays greppable.
        if model.display_name != model.id {
            println!("  {}", model.display_name);
        }
        if let Some(description) = &model.description {
            println!("  {description}");
        }
        if let Some(window) = model.context_window {
            println!("  context: {window} tokens");
        }
        if model.supports_reasoning() {
            let levels: Vec<&str> = model
                .reasoning_efforts
                .iter()
                .map(|effort| effort.as_str())
                .collect();
            let default = model
                .starting_effort()
                .map_or_else(String::new, |effort| format!(" (default: {effort})"));
            println!("  effort: {}{default}", levels.join(", "));
        }
    }
    Ok(())
}

fn doctor(config: Config, composed: Composed) -> Result<()> {
    println!("workspace: {}", config.home.workspace_root);
    println!("home:      {}", config.home.home);
    println!(
        "model:     {} / {}",
        config.model.provider, config.model.model
    );

    println!("\nconfig layers:");
    if config.sources.is_empty() {
        println!("  (none; all defaults)");
    }
    for source in &config.sources {
        println!("  {}", source.describe());
    }

    println!("\nproviders:");
    let routes: Vec<String> = composed.providers.routes().map(str::to_string).collect();
    for route in routes {
        println!("  {route}: {}", credential_status(&composed, &route));
    }

    Ok(())
}

/// Describe a provider's credentials in terms of what to do about them.
///
/// "Not configured" is only useful when it says what would configure it, so a
/// route with a login flow points at `keke login` and a key-only endpoint names
/// the variable to export.
fn credential_status(composed: &Composed, route: &str) -> String {
    if let Some(auth) = composed.auth_for(route) {
        return if auth.has_usable_credential() {
            format!("signed in ({})", auth.snapshot().source)
        } else {
            format!("not signed in — run `keke login {route}`")
        };
    }

    // No registered login flow: the endpoint takes an API key, and
    // `ProviderInfo::env_key` names it. Ask through the credential store rather
    // than the process environment — the key may be in the keyring or the file
    // layer, which reading `env` directly would miss.
    let Ok(provider) = composed.providers.get(route) else {
        return "unknown".to_string();
    };
    let Some(key) = provider.info().env_key.as_deref() else {
        return "no credentials required".to_string();
    };
    let Ok(reference) = CredentialRef::new(key) else {
        return format!("`{key}` is not a usable credential name");
    };

    let auth = ApiKeyAuth::new(reference, Arc::clone(&composed.credentials));
    if auth.has_usable_credential() {
        format!("{key} is set")
    } else {
        format!("not configured — export {key}")
    }
}

/// Inspect installed runtime plugins.
///
/// Listing must never activate anything: resolution locates files and reads
/// manifests, and that is all. A person needs to be able to look at a plugin
/// they do not yet trust.
fn plugin(action: PluginAction, config: Config) -> Result<()> {
    let plugins = crate::plugins::discover(&config.home)?;
    let mut store = crate::plugins::trust_store(&config.home)?;

    match action {
        PluginAction::List => {
            if plugins.is_empty() {
                println!("no plugins installed");
                println!("\nkeke reads plugins from:");
                println!("  {}/plugins", config.home.home);
                println!("  ~/.claude/plugins");
                println!("  {}/.keke/plugins", config.home.workspace_root);
                println!("  {}/.claude/plugins", config.home.workspace_root);
                return Ok(());
            }

            for plugin in plugins.plugins() {
                let version = plugin.version.as_deref().unwrap_or("no version");
                let trust = store.evaluate(plugin);
                println!("{} ({version}) [{}, {trust}]", plugin.name, plugin.scope);
                if let Some(description) = &plugin.description {
                    println!("  {description}");
                }
                println!(
                    "  {} skills, {} commands, {} hooks, {} mcp servers",
                    plugin.skills.len(),
                    plugin.commands.len(),
                    plugin.hooks.len(),
                    plugin.mcp_servers.len(),
                );

                // Anything keke cannot honor is said out loud here rather than
                // left for the person to discover as silence.
                for kind in &plugin.unsupported {
                    println!("  ! `{kind}` is not implemented by keke and does nothing");
                }
                let inert = plugin.inert_hooks().count();
                if inert > 0 {
                    println!("  ! {inert} hook(s) bound to events keke does not run");
                }
                if !trust.permits_running() {
                    println!(
                        "  ! its programs will not run — `keke plugin trust {}` to allow them",
                        plugin.name
                    );
                }
            }
        }
        PluginAction::Show { name } => {
            let plugin = plugins
                .get(&name)
                .with_context(|| format!("no plugin named `{name}` is installed"))?;

            println!("{} [{}]", plugin.name, plugin.scope);
            println!("root: {}", plugin.root);
            println!("trust: {}", store.evaluate(plugin));
            if let Some(version) = &plugin.version {
                println!("version: {version}");
            }
            if let Some(description) = &plugin.description {
                println!("description: {description}");
            }

            if !plugin.skills.is_empty() {
                println!("\nskills:");
                for skill in &plugin.skills {
                    println!("  {}:{} — {}", skill.plugin, skill.name, skill.description);
                }
            }
            if !plugin.commands.is_empty() {
                println!("\ncommands:");
                for command in &plugin.commands {
                    println!(
                        "  {}:{} — {}",
                        command.plugin, command.name, command.description
                    );
                }
            }
            if !plugin.mcp_servers.is_empty() {
                println!("\nmcp servers:");
                for server in &plugin.mcp_servers {
                    println!(
                        "  {}: {} {}",
                        server.name,
                        server.command,
                        server.args.join(" ")
                    );
                    // Names only. A value here could be a secret, and this
                    // output is the kind of thing people paste into an issue.
                    for (key, _) in &server.env {
                        println!("    env {key}");
                    }
                }
            }
            if !plugin.hooks.is_empty() {
                println!("\nhooks:");
                for hook in &plugin.hooks {
                    let matcher = if hook.matcher.is_empty() {
                        "*"
                    } else {
                        &hook.matcher
                    };
                    let inert = if hook.event.is_supported() {
                        ""
                    } else {
                        "  (keke does not run this event)"
                    };
                    println!("  {} [{matcher}] {}{inert}", hook.event, hook.command);
                }
            }
            for kind in &plugin.unsupported {
                println!("\n! `{kind}` is not implemented by keke and does nothing");
            }
        }
        PluginAction::Trust { name } => {
            let plugin = plugins
                .get(&name)
                .with_context(|| format!("no plugin named `{name}` is installed"))?;
            let executables = plugin.executables();

            if executables.is_empty() {
                println!("`{name}` runs no programs; there is nothing to trust");
                return Ok(());
            }

            // Printed before it takes effect, not after. What is being approved
            // is these lines, and a person cannot approve what they were not
            // shown.
            println!("trusting `{name}` allows it to run:");
            for line in &executables {
                println!("  {line}");
            }

            store.approve(plugin);
            crate::plugins::save_trust_store(&config.home, &store)?;
            println!("\n`{name}` is trusted. Adding to what it runs revokes this.");
        }
        PluginAction::Add {
            source,
            git_ref,
            plugin: wanted,
            yes,
        } => {
            add_plugin(
                &config,
                &mut store,
                &source,
                git_ref.as_deref(),
                wanted.as_deref(),
                yes,
            )?;
        }
        PluginAction::Update { name } => {
            update_plugins(&config, &mut store, &plugins, name.as_deref())?;
        }
        PluginAction::Remove { name } => {
            let plugin = plugins
                .get(&name)
                .with_context(|| format!("no plugin named `{name}` is installed"))?;

            // Only what keke installed is keke's to delete. A plugin the person
            // placed by hand, or one the repository ships, is removed the way it
            // arrived.
            let managed = crate::plugins::install_dir(&config.home);
            if !plugin.root.as_path().starts_with(&managed) {
                bail!(
                    "`{name}` was not installed by keke (it is at {}); remove it the way it got there",
                    plugin.root
                );
            }

            std::fs::remove_dir_all(plugin.root.as_path())
                .with_context(|| format!("removing {}", plugin.root))?;
            store.forget(&plugin.root);
            crate::plugins::save_trust_store(&config.home, &store)?;
            println!("removed `{name}` and forgot what was decided about it");
        }
        PluginAction::Untrust { name } => {
            let plugin = plugins
                .get(&name)
                .with_context(|| format!("no plugin named `{name}` is installed"))?;

            if store.revoke(plugin) {
                crate::plugins::save_trust_store(&config.home, &store)?;
                println!("`{name}` is no longer trusted; its programs will not run");
            } else {
                println!("`{name}` was not trusted; nothing changed");
            }
        }
    }
    Ok(())
}

/// Install one plugin from a git URL or a directory.
///
/// Fetching happens into a staging directory, and nothing reaches the person's
/// plugin directory until the contents have resolved cleanly and been approved.
/// A source that turns out to be broken, or an approval that is declined, must
/// leave nothing behind.
fn add_plugin(
    config: &Config,
    store: &mut keke_plugin::TrustStore,
    source: &str,
    git_ref: Option<&str>,
    wanted: Option<&str>,
    assumed_yes: bool,
) -> Result<()> {
    let staging = tempfile::tempdir().context("making a staging directory")?;
    let fetched = staging.path().join("source");
    let local = std::path::Path::new(source);

    let from_git = if local.is_dir() {
        crate::install::copy_tree(local, &fetched)?;
        false
    } else {
        crate::install::clone(source, git_ref, &fetched)?;
        true
    };

    let fetched_abs = AbsPath::new(&fetched).context("staging path")?;
    let revision = crate::install::revision(&fetched);

    // A source may hold one plugin or a catalog of many. Which it is decides
    // what the person is being asked about, so it is settled before anything is
    // shown to them.
    let (package, entry_name) = match keke_plugin::Marketplace::load(&fetched_abs)? {
        None => (fetched.clone(), None),
        Some(catalog) => {
            let Some(wanted) = wanted else {
                println!(
                    "`{}` is a catalog of {} plugins:",
                    catalog.name,
                    catalog.entries.len()
                );
                for entry in &catalog.entries {
                    let description = entry.description.as_deref().unwrap_or("");
                    println!("  {} — {description}", entry.name);
                }
                for name in &catalog.skipped {
                    println!(
                        "  ! {name} — listed with no usable source, so keke cannot install it"
                    );
                }
                bail!("name one with --plugin <name>");
            };
            // An entry the catalog dropped for having no usable source would
            // otherwise be reported as "no such plugin", sending the person to
            // look for a typo in a name that is spelled correctly.
            if catalog.skipped.iter().any(|name| name == wanted) {
                bail!(
                    "`{}` lists `{wanted}` but does not say where to get it; that is the catalog's bug to fix",
                    catalog.name
                );
            }
            let entry = catalog
                .get(wanted)
                .with_context(|| format!("`{}` offers no plugin named `{wanted}`", catalog.name))?;
            match &entry.source {
                keke_plugin::EntrySource::Local { path } => (
                    fetched.join(path.trim_start_matches("./")),
                    Some(wanted.to_string()),
                ),
                keke_plugin::EntrySource::Git { url, reference } => {
                    let nested = staging.path().join("entry");
                    let reference = match reference {
                        keke_plugin::GitRef::Pinned(sha) => Some(sha.clone()),
                        keke_plugin::GitRef::Moving(name) => Some(name.clone()),
                        keke_plugin::GitRef::Default => None,
                    };
                    crate::install::clone(url, reference.as_deref(), &nested)?;
                    (nested, Some(wanted.to_string()))
                }
            }
        }
    };

    let package = AbsPath::new(&package).context("the plugin directory inside the source")?;
    let plugin = keke_plugin::load(package.as_path(), keke_plugin::PluginScope::User)
        .with_context(|| format!("reading the plugin at {package}"))?;

    if !crate::plugins::confirm_executables(&plugin, assumed_yes)? {
        bail!("not installed");
    }

    let target = crate::plugins::install_dir(&config.home).join(&plugin.name);
    std::fs::create_dir_all(crate::plugins::install_dir(&config.home))
        .context("creating the plugin directory")?;
    crate::install::swap_in(package.as_path(), &target)?;

    let target = AbsPath::new(&target).context("the installed path")?;
    let moving = git_ref.is_none_or(|reference| !looks_like_a_commit(reference));
    let install_source = if from_git {
        match entry_name {
            Some(entry) => keke_plugin::InstallSource::Marketplace {
                url: source.to_string(),
                catalog: source.to_string(),
                entry,
                reference: git_ref.map(str::to_string),
                moving,
            },
            None => keke_plugin::InstallSource::Git {
                url: source.to_string(),
                reference: git_ref.map(str::to_string),
                moving,
            },
        }
    } else {
        keke_plugin::InstallSource::Path {
            path: source.to_string(),
        }
    };
    let can_change = install_source.can_change_under_you();

    store.record_install(&target, &plugin.name, install_source, revision);
    // Approval is recorded against the installed path, so it is taken after the
    // move: the record has to describe where the plugin actually is.
    let installed = keke_plugin::load(target.as_path(), keke_plugin::PluginScope::User)
        .with_context(|| format!("reading the installed plugin at {target}"))?;
    store.approve(&installed);
    crate::plugins::save_trust_store(&config.home, store)?;

    println!("installed `{}` into {target}", plugin.name);
    if can_change {
        println!(
            "note: this source can point somewhere else later — `keke plugin update` will ask again if what it runs changes"
        );
    }
    Ok(())
}

/// Whether a ref names a commit rather than a branch or tag.
///
/// A guess, and only used to describe the source in the record. Getting it
/// wrong describes a pin as moving, which asks the person one question too
/// many; the opposite would be the dangerous direction, so the test is strict.
fn looks_like_a_commit(reference: &str) -> bool {
    reference.len() >= 7
        && reference.len() <= 40
        && reference.chars().all(|c| c.is_ascii_hexdigit())
}

/// Fetch installed plugins again.
///
/// The point of this command is the check at the end, not the fetch: an update
/// that changes what a plugin runs withdraws the approval that covered the old
/// contents. Otherwise `update` would be the way to get code onto a machine
/// without anyone looking at it, which is the hole the whole gate exists for.
fn update_plugins(
    config: &Config,
    store: &mut keke_plugin::TrustStore,
    plugins: &keke_plugin::PluginSet,
    only: Option<&str>,
) -> Result<()> {
    let mut updated = 0;
    for plugin in plugins.plugins() {
        if only.is_some_and(|name| name != plugin.name) {
            continue;
        }
        let Some(record) = store.record(plugin) else {
            continue;
        };
        let Some(source) = record.installed.clone() else {
            continue;
        };
        let Some(url) = source.git_url() else {
            println!(
                "`{}` was installed from a directory; nothing to fetch",
                plugin.name
            );
            continue;
        };

        let before = record.revision.clone();
        let staging = tempfile::tempdir().context("making a staging directory")?;
        let fetched = staging.path().join("source");
        crate::install::clone(url, source.git_ref(), &fetched)?;
        let after = crate::install::revision(&fetched);

        if after.is_some() && after == before {
            println!(
                "`{}` is already at {}",
                plugin.name,
                before.unwrap_or_default()
            );
            continue;
        }

        crate::install::swap_in(&fetched, plugin.root.as_path())?;
        let refreshed = keke_plugin::load(plugin.root.as_path(), plugin.scope)
            .with_context(|| format!("reading the updated plugin at {}", plugin.root))?;
        store.record_install(&plugin.root, &refreshed.name, source, after.clone());
        updated += 1;

        println!(
            "updated `{}`{}",
            plugin.name,
            after
                .as_deref()
                .map(|r| format!(" to {r}"))
                .unwrap_or_default()
        );
        match store.evaluate(&refreshed) {
            keke_plugin::Trust::Approved | keke_plugin::Trust::NothingToRun => {}
            _ => {
                println!("  what it runs changed, so it is no longer trusted:");
                for line in refreshed.executables() {
                    println!("    {line}");
                }
                println!("  `keke plugin trust {}` to allow it again", refreshed.name);
            }
        }
    }
    crate::plugins::save_trust_store(&config.home, store)?;
    if updated == 0 && only.is_none() {
        println!("nothing to update");
    }
    Ok(())
}

/// The model a declared provider says it serves when nothing else chose one.
///
/// `[providers.<route>] default_model` is this deployment's own answer to that
/// question — first_run asks for it and stores it — so it outranks whatever
/// happens to head the vendor's model list.
fn declared_default_model(config: &Config, route: &str) -> Option<String> {
    config
        .providers
        .iter()
        .find(|declared| declared.route == route)
        .and_then(|declared| declared.default_model.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(route: &str, default_model: Option<&str>) -> Config {
        let layer = keke_config::ConfigLayer::parse(
            keke_config::LayerSource::Inline("test".to_string()),
            &format!(
                "[providers.{route}]\nbase_url = \"https://gw.example/v1\"\n{}",
                match default_model {
                    Some(model) => format!("default_model = \"{model}\"\n"),
                    None => String::new(),
                }
            ),
        )
        .expect("parses");
        Config::from_layers(
            HomeLayout {
                home: keke_paths::AbsPath::new("/home").expect("abs"),
                workspace_root: keke_paths::AbsPath::new("/ws").expect("abs"),
            },
            &[layer],
        )
        .expect("merges")
    }

    #[test]
    fn a_declared_providers_default_model_is_found_by_route() {
        let config = config_with("openrouter", Some("anthropic/claude-sonnet-4"));
        assert_eq!(
            declared_default_model(&config, "openrouter").as_deref(),
            Some("anthropic/claude-sonnet-4")
        );
    }

    #[test]
    fn an_undeclared_or_unspecified_route_has_no_default() {
        assert_eq!(
            declared_default_model(&config_with("openrouter", None), "openrouter"),
            None
        );
        assert_eq!(
            declared_default_model(&config_with("openrouter", Some("m/a")), "nvidia"),
            None
        );
    }
}
