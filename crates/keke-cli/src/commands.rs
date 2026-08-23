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
    // override possible without editing a file.
    if let Some(provider) = cli.provider {
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
        interactive.then(|| Arc::clone(&approvals)),
    )?;

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
        Command::Resume(args) => resume(args, config, composed, cwd, approvals, requests).await,
        Command::Exec(args) => exec(args, config, composed, cwd).await,
        Command::Agent { transport } => agent(transport, config, cwd).await,
        Command::Login(args) => login(args, composed).await,
        Command::Logout(args) => logout(args, composed).await,
        Command::Models(args) => models(args, composed).await,
        Command::Doctor => doctor(config, composed),
        Command::Plugin { action } => plugin(action, config),
    }
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
    if auth
        .as_ref()
        .is_some_and(|auth| !auth.has_usable_credential())
    {
        bail!("not signed in to `{route}`; run `keke login {route}`");
    }

    let mut builder = SessionBuilder::new()
        .config(SessionConfig {
            model: ModelSelection {
                provider: route,
                model: config.model.model.clone(),
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
    let factory = Arc::new(EditorSessions { config, cwd });
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
}

impl keke_acp::SessionFactory for EditorSessions {
    fn open(
        &self,
        cwd: std::path::PathBuf,
    ) -> keke_acp::ConversationFuture<
        '_,
        Result<
            (
                Arc<dyn keke_acp::Conversation>,
                tokio::sync::mpsc::UnboundedReceiver<keke_acp::Update>,
            ),
            keke_acp::ConversationError,
        >,
    > {
        Box::pin(async move {
            // The client names the directory; keke's own `--cwd` is the
            // fallback for a client that does not.
            let cwd = if cwd.as_os_str().is_empty() {
                self.cwd.clone()
            } else {
                cwd
            };
            let (approvals, requests) = keke_acp::approvals();
            let composed = Composed::build(
                &self.config.home,
                &self.config.providers,
                self.config.plugins,
                Some(Arc::clone(&approvals)),
            )
            .map_err(|error| keke_acp::ConversationError::Agent(error.to_string()))?;
            let builder =
                session_builder(&self.config, &composed, cwd, self.config.approval_policy)
                    .await
                    .map_err(|error| keke_acp::ConversationError::Agent(error.to_string()))?;
            keke_acp::local(builder, approvals, requests)
                .await
                .map_err(|error| keke_acp::ConversationError::Agent(error.to_string()))
        })
    }
}

/// Reopen a previous session, or list what there is to reopen.
///
/// The history comes from the rollout log and nowhere else: what keke can
/// replay is what keke can continue, so there is no second record for the two
/// to disagree about.
async fn resume(
    args: ResumeArgs,
    config: Config,
    composed: Composed,
    cwd: std::path::PathBuf,
    approvals: Arc<keke_acp::Approvals>,
    requests: keke_acp::ApprovalRequests,
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
    let notice = format!(
        "resumed session {id} — {} message(s), {} tokens so far",
        resumed.history.len(),
        resumed.usage.total()
    );
    let seed = keke_tui::Resumed {
        history: resumed.history.clone(),
        usage: resumed.usage,
        notice: Some(notice),
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
    // Read before the session moves `cwd`: the typing history belongs to the
    // directory being worked in, which for a resumed session is the one it was
    // started in rather than wherever keke was invoked.
    let prompts = prompt_history(&config.home.home, &cwd, resume.as_ref().map(|(id, _)| *id));
    let mut builder = session_builder(&config, &composed, cwd, config.approval_policy).await?;
    if let Some((id, history)) = resume {
        builder = builder.resume(id, history);
    }
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
    let (conversation, updates) = keke_acp::local(builder, approvals, requests).await?;
    keke_tui::run(
        conversation,
        updates,
        keke_tui::SlashCommands::new(commands),
        config.approval_policy,
        config.reasoning_effort,
        seed,
        prompts,
    )
    .await
}

/// This project's past prompts, plus the sink new ones are appended to.
///
/// A history that cannot be read is an empty one: somebody opening keke wants
/// their session, not a startup failure over a convenience file.
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

    let streaming = is_interactive();
    let renderer = tokio::spawn(async move {
        let mut out = std::io::stdout();
        while let Some(update) = rx.recv().await {
            match update {
                TurnUpdate::TextDelta { delta, .. } if streaming => {
                    let _ = write!(out, "{delta}");
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
        let context = model
            .context_window
            .map(|window| format!("  {window} tokens"))
            .unwrap_or_default();
        println!("{}{context}", model.id);
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
