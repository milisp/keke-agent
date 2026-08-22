//! The command grammar.

use std::path::PathBuf;

use clap::Parser;
use clap::Subcommand;

/// keke — a multi-vendor terminal coding agent.
#[derive(Debug, Parser)]
#[command(name = "keke", version, about, disable_help_subcommand = true)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Directory to work in. Defaults to the current directory.
    #[arg(long, short = 'C', global = true)]
    pub cwd: Option<PathBuf>,

    /// Provider route to use, overriding configuration.
    #[arg(long, global = true, env = "KEKE_PROVIDER")]
    pub provider: Option<String>,

    /// Model to use, overriding configuration.
    #[arg(long, global = true, env = "KEKE_MODEL")]
    pub model: Option<String>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Run one prompt to completion and print the reply.
    Exec(ExecArgs),
    /// Authenticate with a provider.
    Login(LoginArgs),
    /// Discard stored credentials for a provider.
    Logout(VendorArgs),
    /// List the models a provider can serve.
    Models(VendorArgs),
    /// Report what keke resolved: config, credentials, and available tools.
    Doctor,
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
