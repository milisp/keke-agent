//! Command implementations.
//!
//! `run` resolves configuration and composes the vendor set, then hands off to
//! one surface per command. The helpers below are what more than one surface
//! needs, and live here so the surfaces cannot drift apart on them.

mod acp;
mod exec;
mod mcp;
mod plugin;
mod session;
mod vendor;

use std::sync::Arc;

use anyhow::Context as _;
use anyhow::Result;
use anyhow::bail;
use keke_auth_api::CredentialRef;
use keke_config::Config;
use keke_config_types::HomeLayout;
use keke_config_types::ModelSelection;
use keke_core::SessionBuilder;
use keke_core::SessionConfig;

use crate::cli::Cli;
use crate::cli::Command;
use crate::compose::Composed;
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

    // Made once, here, and shared: `keke_plan::install` enforces plan mode
    // through this cell and `session_builder` builds the session around the
    // same one, which is what lets a person's toggle and the extension's own
    // transitions be the same fact rather than two that can disagree.
    let mode = Arc::new(keke_core::SessionModeSwitch::new(if cli.plan {
        keke_config_types::SessionMode::Plan
    } else {
        keke_config_types::SessionMode::Default
    }));
    let composed = Composed::build(
        &config.home,
        &config.providers,
        config.plugins,
        config.model_catalog_ttl,
        config.subagents,
        interactive.then(|| Arc::clone(&approvals)),
        Some(crate::compose::PlanSetup::for_session(
            &config.home.home,
            &cwd,
            Arc::clone(&mode),
        )),
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
                    Some(crate::compose::PlanSetup::for_session(
                        &config.home.home,
                        &cwd,
                        Arc::clone(&mode),
                    )),
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
            session::tui(
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
            session::resume(
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
        Command::Exec(args) => exec::exec(args, config, composed, cwd).await,
        Command::Agent { transport } => acp::agent(transport, config, cwd).await,
        Command::Login(args) => vendor::login(args, composed).await,
        Command::Logout(args) => vendor::logout(args, composed).await,
        Command::Models(args) => vendor::models(args, composed).await,
        Command::Doctor => vendor::doctor(config, composed),
        Command::Mcp { action } => mcp::mcp(action, config).await,
        Command::Plugin { action } => plugin::plugin(action, config),
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

    // The same cell `keke_plan::install` enforces against, so `Session::
    // session_mode` and the guards can never give different answers.
    if let Some(mode) = &composed.plan_mode {
        builder = builder.mode_switch(Arc::clone(mode));
    }

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
