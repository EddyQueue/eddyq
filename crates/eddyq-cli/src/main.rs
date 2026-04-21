//! `eddyq` — command-line interface for the eddyq job queue.

#![forbid(unsafe_code)]

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "eddyq",
    version,
    about = "eddyq — Rust + Postgres job queue",
    long_about = None,
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run pending schema migrations against the configured database.
    Migrate,
    /// Inspect or manage jobs.
    Jobs,
    /// Inspect or manage concurrency groups.
    Groups,
    /// Stop accepting new work and wait for in-flight jobs to finish.
    Drain,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "eddyq=info".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Migrate => unimplemented!("migrate — Phase 1"),
        Command::Jobs => unimplemented!("jobs — Phase 1"),
        Command::Groups => unimplemented!("groups — Phase 2"),
        Command::Drain => unimplemented!("drain — Phase 1"),
    }
}
