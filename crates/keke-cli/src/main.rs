//! The `keke` binary.
//!
//! Deliberately thin. Everything it does is delegated: the grammar to
//! [`cli`], the vendor wiring to [`compose`], the work to `keke-core`. The
//! reference implementations both grew three-thousand-line entry points, and
//! that is the shape being avoided.

mod api_key;
mod cli;
mod commands;
mod compose;
mod declared;
mod first_run;
mod install;
mod plugins;
mod ui;

use anyhow::Result;
use clap::Parser;

/// Where logs go when the terminal is not ours to write on: `$KEKE_HOME/log/keke.log`.
fn log_file() -> Option<std::fs::File> {
    let home = match std::env::var("KEKE_HOME") {
        Ok(raw) if !raw.is_empty() => std::path::PathBuf::from(raw),
        _ => dirs::home_dir()?.join(".keke"),
    };
    let dir = home.join("log");
    std::fs::create_dir_all(&dir).ok()?;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("keke.log"))
        .ok()
}

fn main() -> Result<()> {
    // Parsed before logging is wired: which surface is about to run decides
    // where a log line may go.
    let cli = cli::Cli::parse();

    let filter = tracing_subscriber::EnvFilter::try_from_env("KEKE_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    // The interface owns the whole screen, so a warning written to stderr
    // lands in the middle of it — inside the composer, over the transcript —
    // and nothing redraws it away. Those runs log to a file instead; a plain
    // command still writes to the terminal the person is watching.
    let interactive = matches!(
        cli.command,
        None | Some(cli::Command::Tui) | Some(cli::Command::Resume(_))
    );
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    match interactive.then(log_file).flatten() {
        Some(file) => builder
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(file))
            .init(),
        // No file to log to: silence beats corrupting the interface.
        None if interactive => builder.with_writer(std::io::sink).init(),
        None => builder.with_writer(std::io::stderr).init(),
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(commands::run(cli))
}
