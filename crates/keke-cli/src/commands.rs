//! Command implementations.

use std::io::Read;
use std::io::Write;
use std::sync::Arc;

use anyhow::Context as _;
use anyhow::Result;
use anyhow::bail;
use keke_config::Config;
use keke_config_types::HomeLayout;
use keke_config_types::ModelSelection;
use keke_core::SessionBuilder;
use keke_core::SessionConfig;
use keke_core::TurnUpdate;
use keke_protocol::Message;
use keke_protocol::StopReason;

use crate::cli::Cli;
use crate::cli::Command;
use crate::cli::ExecArgs;
use crate::cli::LoginArgs;
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

    let composed = Composed::build(&config.home.home)?;

    match cli.command {
        Command::Exec(args) => exec(args, config, composed, cwd).await,
        Command::Login(args) => login(args, composed).await,
        Command::Logout(args) => logout(args, composed).await,
        Command::Models(args) => models(args, composed).await,
        Command::Doctor => doctor(config, composed),
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

    let route = config.model.provider.clone();
    // `context` would make the hint the headline and the failure the cause,
    // which reads backwards; the failure is what happened.
    let provider = composed.providers.get(&route).map_err(|error| {
        anyhow::anyhow!(
            "{error}\n\navailable providers: {}",
            composed.providers.routes().collect::<Vec<_>>().join(", ")
        )
    })?;

    let auth = composed.auth_for(&route);
    // Checked before the turn starts: discovering this after a rollout log has
    // been opened and a request built is a worse experience than one line here.
    if auth
        .as_ref()
        .is_some_and(|auth| !auth.has_usable_credential())
    {
        bail!("not signed in to `{route}`; run `keke login {route}`");
    }

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut builder = SessionBuilder::new()
        .config(SessionConfig {
            model: ModelSelection {
                provider: route.clone(),
                model: config.model.model.clone(),
            },
            home: HomeLayout {
                home: config.home.home.clone(),
                workspace_root: config.home.workspace_root.clone(),
            },
        })
        .provider(provider)
        .extensions(composed.extensions.clone())
        .cwd(cwd)
        .updates(tx);

    if let Some(auth) = auth {
        builder = builder.auth(auth);
    }

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
    println!("Signed out of {}.", args.vendor);
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
    // `ProviderInfo::env_key` names it.
    let Ok(provider) = composed.providers.get(route) else {
        return "unknown".to_string();
    };
    match provider.info().env_key.as_deref() {
        Some(key) if std::env::var_os(key).is_some() => format!("{key} is set"),
        Some(key) => format!("not configured — export {key}"),
        None => "no credentials required".to_string(),
    }
}
