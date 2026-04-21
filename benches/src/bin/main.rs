//! `eddyq-bench` — orchestrator binary for the benchmark harness.
//!
//! Usage:
//!   eddyq-bench run --scenario throughput-10k --system eddyq
//!   eddyq-bench run --all   # runs every scenario against every system, emits bench-report.md

#![forbid(unsafe_code)]

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "eddyq-bench", about = "eddyq benchmark harness", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run benchmark scenarios and emit bench-report.md.
    Run {
        /// Scenario name, or "all" to run the canonical set.
        #[arg(long, default_value = "all")]
        scenario: String,
        /// System under test: eddyq, sqlxmq, graphile, bullmq, or "all".
        #[arg(long, default_value = "all")]
        system: String,
    },
    /// List available scenarios.
    Scenarios,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "eddyq_benches=info,info".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Run { scenario, system } => {
            tracing::info!(%scenario, %system, "bench harness not yet implemented");
            unimplemented!("harness runner — wired up once eddyq-core Phase 1 lands")
        }
        Command::Scenarios => {
            for s in eddyq_benches::scenarios::release_scenarios() {
                println!(
                    "{:<30} jobs={:<6} workers={:<4} payload={}B kind={:?}",
                    s.name, s.n_jobs, s.n_workers, s.payload_size_bytes, s.kind
                );
            }
            Ok(())
        }
    }
}
