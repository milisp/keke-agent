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
mod ui;

use anyhow::Result;
use clap::Parser;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("KEKE_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = cli::Cli::parse();

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(commands::run(cli))
}
