//! The command grammar.

use std::path::PathBuf;

use clap::Parser;
use clap::Subcommand;

/// keke — a multi-vendor terminal coding agent.
#[derive(Debug, Parser)]
#[command(name = "keke", version, about, disable_help_subcommand = true)]
pub(crate) struct Cli {
    /// Omitted, keke opens the interactive interface.
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Directory to work in. Defaults to the current directory.
    #[arg(long, short = 'C', global = true)]
    pub cwd: Option<PathBuf>,

    /// Provider route to use, overriding configuration.
    #[arg(long, global = true, env = "KEKE_PROVIDER")]
    pub provider: Option<String>,

    /// Model to use, overriding configuration.
    #[arg(long, global = true, env = "KEKE_MODEL")]
    pub model: Option<String>,

    /// How hard the model should think: `low`, `medium`, `high`, `xhigh`, or
    /// `max`. Overrides configuration; omitted, the vendor's own default
    /// stands, which is not the same as asking for the least on offer.
    #[arg(long, global = true, env = "KEKE_REASONING_EFFORT", value_parser = parse_effort)]
    pub reasoning_effort: Option<keke_config_types::ReasoningEffort>,

    /// Start in plan mode: research and propose, editing nothing but the plan
    /// file, until the plan is approved. A startup flag rather than a
    /// configuration field because plan mode is about the task in front of you,
    /// not about how this machine is set up.
    #[arg(long, global = true)]
    pub plan: bool,
}

/// Parse an effort level without making the contract crate depend on clap. A
/// level this build does not know is refused here rather than sent on, so the
/// error names the flag instead of arriving as a vendor's rejection.
fn parse_effort(raw: &str) -> Result<keke_config_types::ReasoningEffort, String> {
    keke_config_types::ReasoningEffort::parse(raw)
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Open the interactive interface. The default when no command is given.
    #[command(hide = true)]
    Tui,
    /// Reopen a previous session and keep talking.
    Resume(ResumeArgs),
    /// Run one prompt to completion and print the reply.
    Exec(ExecArgs),
    /// Serve the Agent Client Protocol on stdin and stdout, for an editor.
    Agent {
        #[command(subcommand)]
        transport: AgentTransport,
    },
    /// Authenticate with a provider.
    Login(LoginArgs),
    /// Discard stored credentials for a provider.
    Logout(VendorArgs),
    /// List the models a provider can serve.
    Models(VendorArgs),
    /// Report what keke resolved: config, credentials, and available tools.
    Doctor,
    /// Manage the MCP servers keke offers the model as tools.
    Mcp {
        #[command(subcommand)]
        action: McpAction,
    },
    /// Inspect the runtime plugins installed on this machine.
    Plugin {
        #[command(subcommand)]
        action: PluginAction,
    },
}

const MCP_ADD_AFTER_HELP: &str = "\
Examples:
  # A local server: everything after `--` is the command keke runs
  keke mcp add xcode -- xcrun mcpbridge

  # ... with environment it needs, forwarded from your own shell
  keke mcp add postgres -e DATABASE_URL='${DATABASE_URL}' -- npx -y @modelcontextprotocol/server-postgres

  # A remote server
  keke mcp add --transport http vercel https://mcp.vercel.com

  # ... with a header, keeping the token out of the file
  keke mcp add --transport http api https://mcp.example.com/mcp --header 'Authorization: Bearer ${API_TOKEN}'

  # Written into the project instead of your own directory. A server configured
  # there is the repository's, so it is held back until it is trusted.
  keke mcp add --scope project github -- npx -y @modelcontextprotocol/server-github";

#[derive(Debug, clap::Subcommand)]
pub(crate) enum McpAction {
    /// Configure a server.
    #[command(after_help = MCP_ADD_AFTER_HELP)]
    Add(McpAddArgs),
    /// List every server keke would offer, wherever it was configured.
    List,
    /// Sign in to a remote server, in a browser.
    ///
    /// Only ever run because a person asked: a turn that opened a browser by
    /// itself would take over the screen of someone who was reading something
    /// else.
    Login {
        /// The server's name, as `list` prints it.
        name: String,
    },
    /// Discard the token stored for a remote server.
    Logout {
        /// The server's name, as `list` prints it.
        name: String,
    },
    /// Show one server in full.
    Get {
        /// The server's name, as `list` prints it.
        name: String,
    },
    /// Forget a server keke configured. A plugin's own server is the plugin's.
    Remove {
        /// The server's name, as `list` prints it.
        name: String,
        /// Which file to remove it from. Both are searched if omitted.
        #[arg(long, value_enum)]
        scope: Option<McpScope>,
    },
    /// Leave a server configured but stop starting it.
    Disable {
        /// The server's name, as `list` prints it.
        name: String,
    },
    /// Undo `disable`.
    Enable {
        /// The server's name, as `list` prints it.
        name: String,
    },
}

/// How a server is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum McpTransportArg {
    /// Run a program here and speak over its stdin and stdout.
    Stdio,
    /// Connect to a URL with the streamable HTTP transport.
    Http,
    /// Connect to a URL with the older HTTP+SSE transport.
    Sse,
}

/// Which file a server is written to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum McpScope {
    /// Your own directory: available in every project.
    User,
    /// `.keke/.mcp.json` in this project, shared with whoever clones it.
    Project,
}

#[derive(Debug, clap::Args)]
pub(crate) struct McpAddArgs {
    /// What the server is called. Its tools are offered under this name.
    pub name: String,

    /// The URL, for `--transport http` or `--transport sse`.
    pub url: Option<String>,

    /// The program to run, for a stdio server. Everything after `--`.
    #[arg(last = true)]
    pub command: Vec<String>,

    #[arg(long, value_enum, default_value_t = McpTransportArg::Stdio)]
    pub transport: McpTransportArg,

    #[arg(long, value_enum, default_value_t = McpScope::User)]
    pub scope: McpScope,

    /// `KEY=VALUE` passed to a stdio server. A `${VAR}` in the value is read
    /// from your environment when the server starts, so the file holds the
    /// reference and never the secret. Repeatable.
    #[arg(long, short = 'e')]
    pub env: Vec<String>,

    /// `Name: value` sent with every request to a remote server. `${VAR}` is
    /// read the same way. Repeatable.
    #[arg(long = "header", short = 'H')]
    pub headers: Vec<String>,

    /// Replace an existing server of this name without being asked.
    #[arg(long)]
    pub force: bool,

    /// Do not offer to sign in to a remote server after adding it.
    #[arg(long)]
    pub no_login: bool,
}

#[derive(Debug, clap::Subcommand)]
pub(crate) enum PluginAction {
    /// List installed plugins and what each contributes.
    List,
    /// Show one plugin in full, including anything keke cannot honor.
    Show {
        /// The plugin's name, as `list` prints it.
        name: String,
    },
    /// Allow a plugin from the workspace to run the programs it ships.
    Trust {
        /// The plugin's name, as `list` prints it.
        name: String,
    },
    /// Withdraw that permission.
    Untrust {
        /// The plugin's name, as `list` prints it.
        name: String,
    },
    /// Install a plugin from a git repository or a directory.
    Add {
        /// A git URL, or a path to a directory on this machine.
        source: String,
        /// Branch, tag, or commit. A commit cannot change under you later.
        #[arg(long)]
        git_ref: Option<String>,
        /// Which plugin to take, when the source is a catalog of several.
        #[arg(long)]
        plugin: Option<String>,
        /// Approve what it runs without being asked to confirm.
        ///
        /// Scoped to this one command, naming this one source. It is not a
        /// setting, because a setting is what a person turns on once and then
        /// stops seeing.
        #[arg(long)]
        yes: bool,
    },
    /// Fetch installed plugins again.
    Update {
        /// One plugin, or every installed one if omitted.
        name: Option<String>,
    },
    /// Uninstall a plugin and forget what was decided about it.
    Remove {
        /// The plugin's name, as `list` prints it.
        name: String,
    },
}

#[derive(Debug, clap::Args)]
pub(crate) struct ResumeArgs {
    /// Which session — the short id `--list` prints, or any prefix of the full
    /// one. The most recent conversation if omitted.
    pub session: Option<String>,

    /// List what can be resumed instead of resuming anything.
    ///
    /// Shows only sessions started in the current directory; pass `--all` to
    /// see every session under every directory.
    #[arg(long)]
    pub list: bool,

    /// Resume the current directory's most recent session.
    #[arg(long)]
    pub last: bool,

    /// Cover every directory's sessions, not just the current one — and
    /// include sessions nobody said anything in.
    #[arg(long)]
    pub all: bool,
}

#[derive(Debug, clap::Args)]
pub(crate) struct ExecArgs {
    /// The prompt. Reads stdin when omitted.
    pub prompt: Option<String>,

    /// Print the rollout log path on completion.
    #[arg(long)]
    pub print_log_path: bool,

    /// Override the approval policy for this run: `on-request`, `on-failure`,
    /// or `never`.
    ///
    /// `exec` has nobody to ask, so a call needing approval is refused. Pass
    /// `never` for CI, where the confinement is the sandbox rather than a
    /// person.
    #[arg(long, value_parser = parse_approval)]
    pub approval: Option<keke_config_types::ApprovalPolicy>,

    /// Output format: `text` (default) or `json`.
    ///
    /// `json` emits a single JSON object on stdout with the fields `text`,
    /// `stopReason`, and `usage`. Progress lines always go to stderr so a
    /// caller can separate them from the machine-readable result.
    #[arg(long, visible_alias = "output-format", value_enum, default_value_t)]
    pub format: ExecFormat,
}

/// How `keke exec` presents its output.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum ExecFormat {
    /// Human-readable text, optionally streamed when connected to a terminal.
    #[default]
    Text,
    /// A single JSON object written to stdout after the turn completes.
    Json,
}

/// Parse an approval policy without making the contract crate depend on clap.
fn parse_approval(raw: &str) -> Result<keke_config_types::ApprovalPolicy, String> {
    use keke_config_types::ApprovalPolicy;
    match raw {
        "on-request" => Ok(ApprovalPolicy::OnRequest),
        "on-failure" => Ok(ApprovalPolicy::OnFailure),
        "never" => Ok(ApprovalPolicy::Never),
        other => Err(format!(
            "unknown approval policy `{other}`; expected on-request, on-failure, or never"
        )),
    }
}

#[derive(Debug, Subcommand)]
pub(crate) enum AgentTransport {
    /// Speak ACP over stdin and stdout. The transport every editor uses today.
    Stdio,
}

#[derive(Debug, clap::Args)]
pub(crate) struct LoginArgs {
    /// Which vendor to authenticate with, e.g. `codex` or `grok`.
    #[arg(default_value = "grok")]
    pub vendor: String,

    /// Use the device-code flow instead of a browser redirect. Needed when
    /// keke cannot reach a browser, as on a remote host.
    #[arg(long)]
    pub device_code: bool,
}

#[derive(Debug, clap::Args)]
pub(crate) struct VendorArgs {
    /// Which vendor to act on, e.g. `codex` or `grok`.
    #[arg(default_value = "grok")]
    pub vendor: String,
}
