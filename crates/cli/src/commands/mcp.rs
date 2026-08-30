//! Configuring MCP servers without authoring a plugin.
//!
//! What `keke mcp add` writes is an ordinary `.mcp.json`, in the directory the
//! rest of keke already reads plugin content from. That is the whole design:
//! there is no second format, no second discovery path, and a server added here
//! is indistinguishable afterwards from one a plugin shipped — including in
//! how it is trusted. A server written into the project is content the
//! repository carries, so it is held back until a person approves it
//! (`AGENTS.md` invariant 13), exactly as a plugin's would be.

use anyhow::Context as _;
use anyhow::Result;
use anyhow::bail;
use keke_config::Config;
use keke_config_types::HomeLayout;
use keke_plugin::McpDocument;
use keke_plugin::McpTransport;

use crate::cli::McpAction;
use crate::cli::McpAddArgs;
use crate::cli::McpScope;
use crate::cli::McpTransportArg;

pub(super) async fn mcp(action: McpAction, config: Config) -> Result<()> {
    match action {
        McpAction::Add(args) => add(args, &config.home).await,
        McpAction::List => list(&config.home),
        McpAction::Get { name } => get(&name, &config.home),
        McpAction::Login { name } => login(&name, &config.home).await,
        McpAction::Logout { name } => logout(&name, &config.home),
        McpAction::Remove { name, scope } => remove(&name, scope, &config.home),
        McpAction::Enable { name } => set_disabled(&name, false, &config.home),
        McpAction::Disable { name } => set_disabled(&name, true, &config.home),
    }
}

/// The remote server `name` refers to, and the URL it is reached at.
///
/// A stdio server has nothing to sign in to, and saying so plainly beats a
/// flow that opens a browser at nothing.
fn remote(name: &str, home: &HomeLayout) -> Result<String> {
    let plugins = crate::plugins::discover(home)?;
    let server = plugins
        .mcp_servers()
        .find(|server| server.name == name)
        .with_context(|| format!("no MCP server named `{name}` is configured"))?;
    match &server.transport {
        McpTransport::Http { url, .. } | McpTransport::Sse { url, .. } => Ok(url.clone()),
        McpTransport::Stdio { .. } => bail!(
            "`{name}` is a program on this machine, not a remote server — there is nothing to sign in to"
        ),
    }
}

fn credential(name: &str, home: &HomeLayout) -> Result<keke_mcp::ServerAuth> {
    let url = remote(name, home)?;
    keke_mcp::ServerAuth::new(keke_mcp::AuthHome::new(&home.home), name, &url)
        .map_err(|error| anyhow::anyhow!("{error}"))
}

async fn login(name: &str, home: &HomeLayout) -> Result<()> {
    credential(name, home)?
        .login(&crate::ui::TerminalLoginUi)
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    println!("signed in to `{name}`");
    Ok(())
}

fn logout(name: &str, home: &HomeLayout) -> Result<()> {
    let discarded = credential(name, home)?
        .logout()
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    if discarded {
        println!("discarded the token for `{name}`");
    } else {
        println!("`{name}` had no stored token");
    }
    Ok(())
}

/// The file a scope's servers live in.
fn file(scope: McpScope, home: &HomeLayout) -> std::path::PathBuf {
    let root = match scope {
        McpScope::User => home.home.as_path().to_path_buf(),
        McpScope::Project => home.workspace_root.as_path().join(".keke"),
    };
    root.join(keke_plugin::MCP_FILE)
}

async fn add(args: McpAddArgs, home: &HomeLayout) -> Result<()> {
    let transport = transport(&args)?;
    let path = file(args.scope, home);
    let mut document = McpDocument::open(&path)?;

    // Replacing a server silently would make `add` a way to lose a working
    // configuration to a typo in a name.
    if document.get(&args.name).is_some() && !args.force {
        bail!(
            "`{}` is already configured in {} — pass --force to replace it",
            args.name,
            path.display()
        );
    }

    let replaced = document.insert(&args.name, transport.clone().into());
    document.save(&path)?;

    println!(
        "{} `{}` in {}",
        if replaced { "replaced" } else { "added" },
        args.name,
        path.display()
    );
    println!("  {}", transport.describe());
    if args.scope == McpScope::Project {
        println!(
            "\nthis is in the project, so keke will not run it until it is trusted:\n  keke plugin trust workspace"
        );
    }

    // Most remote servers want OAuth, and finding that out mid-turn is a worse
    // moment than finding it out now. Offered rather than done: a login opens a
    // browser, which is not something to spring on someone.
    if !transport.is_local() && !args.no_login {
        offer_login(&args.name, home).await?;
    }
    Ok(())
}

/// Ask whether to sign in now, and do it if the answer is yes.
///
/// A non-interactive run declines and says how to do it later — there is
/// nobody there to complete a browser flow anyway.
async fn offer_login(name: &str, home: &HomeLayout) -> Result<()> {
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        println!("\nif it needs authorization: keke mcp login {name}");
        return Ok(());
    }

    print!("\nsign in to `{name}` now? [Y/n] ");
    std::io::Write::flush(&mut std::io::stdout()).context("writing the prompt")?;
    let mut answer = String::new();
    std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut answer)
        .context("reading the answer")?;
    // The default is yes here, unlike the trust prompt: this grants keke
    // nothing, it only asks the person's browser to authorize a server they
    // just chose to add.
    if matches!(answer.trim(), "" | "y" | "Y" | "yes") {
        if let Err(error) = login(name, home).await {
            // Not fatal: the server is configured either way, and a server
            // that needs no authentication answers the discovery with nothing.
            println!("could not sign in: {error}");
            println!("the server is configured; retry with `keke mcp login {name}`");
        }
    } else {
        println!("skipped — `keke mcp login {name}` when you want to");
    }
    Ok(())
}

/// What the flags and positionals add up to, or why they do not.
///
/// The two shapes are checked against each other rather than one being taken
/// on trust: `--transport http` with a trailing command, or a stdio server with
/// a URL, is a person who meant something this command cannot guess at.
fn transport(args: &McpAddArgs) -> Result<McpTransport> {
    match args.transport {
        McpTransportArg::Stdio => {
            let Some((command, rest)) = args.command.split_first() else {
                bail!(
                    "a stdio server needs a command: `keke mcp add {} -- <program> [args...]`",
                    args.name
                );
            };
            if args.url.is_some() {
                bail!(
                    "a stdio server takes no URL — did you mean `--transport http`, or is `{}` meant to be part of the command after `--`?",
                    args.url.as_deref().unwrap_or_default()
                );
            }
            if !args.headers.is_empty() {
                bail!("headers are sent to a remote server; a stdio server takes `-e KEY=VALUE`");
            }
            Ok(McpTransport::Stdio {
                command: command.clone(),
                args: rest.to_vec(),
                env: pairs(&args.env, '=', "-e KEY=VALUE")?,
            })
        }
        McpTransportArg::Http | McpTransportArg::Sse => {
            let url = args.url.clone().with_context(|| {
                format!(
                    "a remote server needs a URL: `keke mcp add --transport {} {} <url>`",
                    if args.transport == McpTransportArg::Http {
                        "http"
                    } else {
                        "sse"
                    },
                    args.name
                )
            })?;
            if !args.command.is_empty() {
                bail!("a remote server runs no command here; drop everything after `--`");
            }
            if !args.env.is_empty() {
                bail!(
                    "environment is handed to a program keke starts; a remote server takes `--header 'Name: value'`"
                );
            }
            let headers = pairs(&args.headers, ':', "--header 'Name: value'")?;
            Ok(if args.transport == McpTransportArg::Http {
                McpTransport::Http { url, headers }
            } else {
                McpTransport::Sse { url, headers }
            })
        }
    }
}

/// Split `KEY=VALUE` or `Name: value` arguments, keeping the order given.
///
/// Split on the *first* separator only: a header value holds colons and an
/// environment value holds `=`, and splitting greedily would quietly truncate
/// the thing the person is trying to pass.
fn pairs(raw: &[String], separator: char, form: &str) -> Result<Vec<(String, String)>> {
    raw.iter()
        .map(|entry| {
            let (key, value) = entry
                .split_once(separator)
                .with_context(|| format!("`{entry}` is not of the form `{form}`"))?;
            let key = key.trim();
            if key.is_empty() {
                bail!("`{entry}` has no name before the `{separator}`");
            }
            Ok((key.to_string(), value.trim().to_string()))
        })
        .collect()
}

/// Every server, as the interface needs to describe it.
///
/// Computed here because this is the layer that can see all three things a row
/// depends on: what is configured, whether it is trusted, and whether a token
/// exists for it. The interface gets text and booleans.
pub(crate) fn statuses(home: &HomeLayout) -> Result<Vec<keke_tui::McpServerStatus>> {
    let plugins = crate::plugins::discover(home)?;
    let store = crate::plugins::trust_store(home)?;
    let auth = keke_mcp::AuthHome::new(&home.home);

    let mut rows = Vec::new();
    for plugin in plugins.plugins() {
        let allowed = store.evaluate(plugin).permits_running();
        for server in &plugin.mcp_servers {
            let remote = !server.transport.is_local();
            let signed_in = match &server.transport {
                McpTransport::Http { url, .. } | McpTransport::Sse { url, .. } => {
                    keke_mcp::ServerAuth::new(auth.clone(), &server.name, url)
                        .is_ok_and(|credential| credential.has_credential())
                }
                McpTransport::Stdio { .. } => false,
            };
            rows.push(keke_tui::McpServerStatus {
                name: server.name.clone(),
                plugin: plugin.name.clone(),
                transport: server.transport.describe(),
                remote,
                signed_in,
                allowed,
                enabled: !server.disabled,
            });
        }
    }
    Ok(rows)
}

/// Signing in from inside the interface.
///
/// Holds the layout rather than a resolved server: the flow re-reads what is
/// configured at the moment a person asks, so a `.mcp.json` edited during a
/// session does not authorize the server it used to name.
pub(crate) struct SignIn {
    pub home: HomeLayout,
}

impl keke_tui::McpSignIn for SignIn {
    fn sign_in(
        &self,
        name: String,
        ui: std::sync::Arc<dyn keke_auth_api::LoginUi>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>> {
        let home = self.home.clone();
        Box::pin(async move {
            let credential = credential(&name, &home).map_err(|error| error.to_string())?;
            credential
                .login(ui.as_ref())
                .await
                .map_err(|error| error.to_string())
        })
    }
}

fn list(home: &HomeLayout) -> Result<()> {
    let plugins = crate::plugins::discover(home)?;
    let store = crate::plugins::trust_store(home)?;

    let mut any = false;
    for plugin in plugins.plugins() {
        if plugin.mcp_servers.is_empty() {
            continue;
        }
        any = true;
        let trust = store.evaluate(plugin);
        println!("{} [{}, {trust}]", plugin.name, plugin.scope);
        for server in &plugin.mcp_servers {
            let disabled = if server.disabled { " (disabled)" } else { "" };
            println!(
                "  {}: {}{disabled}",
                server.name,
                server.transport.describe()
            );
        }
        if !trust.permits_running() {
            println!(
                "  ! not reached until trusted — `keke plugin trust {}`",
                plugin.name
            );
        }
    }

    if !any {
        println!("no MCP servers configured");
        println!("\nadd one with:");
        println!("  keke mcp add --transport http <name> <url>");
        println!("  keke mcp add <name> -- <program> [args...]");
    }
    Ok(())
}

fn get(name: &str, home: &HomeLayout) -> Result<()> {
    let plugins = crate::plugins::discover(home)?;
    let store = crate::plugins::trust_store(home)?;

    let found = plugins
        .plugins()
        .find_map(|plugin| {
            let server = plugin.mcp_servers.iter().find(|s| s.name == name)?;
            Some((plugin, server))
        })
        .with_context(|| format!("no MCP server named `{name}` is configured"))?;
    let (plugin, server) = found;

    println!("{}", server.name);
    println!("transport: {}", server.transport.kind());
    println!("reached by: {}", server.transport.describe());
    println!("configured in: {} [{}]", plugin.name, plugin.scope);
    println!(
        "enabled: {}",
        if server.disabled {
            format!("no — `keke mcp enable {name}`")
        } else {
            "yes".to_string()
        }
    );
    if !server.transport.is_local() {
        let signed_in = credential(name, home).is_ok_and(|auth| auth.has_credential());
        println!(
            "signed in: {}",
            if signed_in {
                "yes".to_string()
            } else {
                format!("no — `keke mcp login {name}`")
            }
        );
    }
    println!("root: {}", plugin.root);
    let trust = store.evaluate(plugin);
    println!("trust: {trust}");
    if !trust.permits_running() {
        println!(
            "\nkeke will not reach it until it is trusted:\n  keke plugin trust {}",
            plugin.name
        );
    }
    Ok(())
}

/// Flip whether a server starts, wherever `keke mcp add` put it.
///
/// Searches both scopes the way `remove` does, for the same reason: a person
/// asking to disable `name` does not know or care which file it landed in.
fn set_disabled(name: &str, disabled: bool, home: &HomeLayout) -> Result<()> {
    for scope in [McpScope::User, McpScope::Project] {
        let path = file(scope, home);
        let mut document = McpDocument::open(&path)?;
        if document.set_disabled(name, disabled) {
            document.save(&path)?;
            println!(
                "{} `{}` in {}",
                if disabled { "disabled" } else { "enabled" },
                name,
                path.display()
            );
            return Ok(());
        }
    }
    bail!(
        "no server named `{name}` was configured by `keke mcp add`\n\nif a plugin contributes it, remove the plugin: `keke plugin remove <name>`"
    );
}

/// Managing servers from inside the interface: toggling, removing, and
/// re-reading the list after either.
///
/// Holds the layout rather than a resolved server for the same reason
/// [`SignIn`] does — a name is re-resolved against whatever `.mcp.json` says
/// at the moment a person acts, not against what it said when `/mcp` opened.
pub(crate) struct Manage {
    pub home: HomeLayout,
}

impl keke_tui::McpManage for Manage {
    fn set_disabled(&self, name: &str, disabled: bool) -> Result<(), String> {
        set_disabled(name, disabled, &self.home).map_err(|error| error.to_string())
    }

    fn remove(&self, name: &str) -> Result<(), String> {
        remove(name, None, &self.home).map_err(|error| error.to_string())
    }

    fn refresh(&self) -> Result<Vec<keke_tui::McpServerStatus>, String> {
        statuses(&self.home).map_err(|error| error.to_string())
    }
}

fn remove(name: &str, scope: Option<McpScope>, home: &HomeLayout) -> Result<()> {
    let scopes: Vec<McpScope> = match scope {
        Some(scope) => vec![scope],
        None => vec![McpScope::User, McpScope::Project],
    };

    let mut removed = false;
    for scope in scopes {
        let path = file(scope, home);
        let mut document = McpDocument::open(&path)?;
        if document.remove(name) {
            document.save(&path)?;
            println!("removed `{name}` from {}", path.display());
            removed = true;
        }
    }

    if !removed {
        // A plugin's server is the plugin's to define, so pointing at the file
        // is not enough — the way to be rid of it is to be rid of the plugin.
        bail!(
            "no server named `{name}` was configured by `keke mcp add`\n\nif a plugin contributes it, remove the plugin: `keke plugin remove <name>`"
        );
    }
    Ok(())
}
