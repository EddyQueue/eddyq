//! `eddyq` — command-line interface for the eddyq job queue.

#![forbid(unsafe_code)]

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use eddyq_core::migrate::{self, Direction};
use sqlx::postgres::PgPoolOptions;

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
    /// Manage schema migrations.
    #[command(subcommand)]
    Migrate(MigrateCmd),
    /// Inspect or manage jobs.
    Jobs,
    /// Inspect or manage concurrency groups.
    Groups,
    /// Stop accepting new work and wait for in-flight jobs to finish.
    Drain,
}

#[derive(Subcommand)]
enum MigrateCmd {
    /// Apply all pending up-migrations. Safe to re-run; already-applied
    /// versions are skipped.
    Run(DbArgs),
    /// List all migrations eddyq knows about, applied and pending.
    List(DbArgs),
    /// Print the raw SQL for a migration. Pipe into your migration tool
    /// (goose / atlas / golang-migrate / sqlx-cli).
    Get(GetArgs),
    /// Roll back the most recent N migrations. Dangerous — drops eddyq tables.
    Down(DownArgs),
}

#[derive(Args)]
struct DbArgs {
    /// Postgres connection URL. Falls back to `$DATABASE_URL`.
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,
    /// Migration line (default: `main`). Each line has its own independent
    /// applied-version history.
    #[arg(long, default_value = "main")]
    line: String,
}

#[derive(Args)]
struct GetArgs {
    /// Migration version (e.g. `20260421000001`).
    version: i64,
    /// Direction: `up` (default) or `down`.
    #[arg(long, default_value = "up")]
    direction: DirectionArg,
}

#[derive(Args)]
struct DownArgs {
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,
    #[arg(long, default_value = "main")]
    line: String,
    /// Maximum number of migrations to roll back.
    #[arg(long, default_value_t = 1)]
    max_steps: usize,
    /// Actually apply the rollback. Without this, prints what would happen.
    #[arg(long)]
    confirm: bool,
}

#[derive(Clone, clap::ValueEnum)]
enum DirectionArg {
    Up,
    Down,
}

impl From<DirectionArg> for Direction {
    fn from(d: DirectionArg) -> Self {
        match d {
            DirectionArg::Up => Direction::Up,
            DirectionArg::Down => Direction::Down,
        }
    }
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
        Command::Migrate(mc) => run_migrate(mc).await,
        Command::Jobs | Command::Groups | Command::Drain => {
            anyhow::bail!("not implemented yet; coming in a later phase")
        }
    }
}

async fn run_migrate(cmd: MigrateCmd) -> Result<()> {
    match cmd {
        MigrateCmd::Run(args) => {
            let pool = connect(&args.database_url).await?;
            let report = migrate::up(&pool, &args.line)
                .await
                .context("migrate up failed")?;
            if report.applied.is_empty() {
                println!("eddyq [line={}]: schema already up to date.", args.line);
            } else {
                println!(
                    "eddyq [line={}]: applied {} migration(s):",
                    args.line,
                    report.applied.len()
                );
                for (v, n) in &report.applied {
                    println!("  + {v}  {n}");
                }
            }
            Ok(())
        }

        MigrateCmd::List(args) => {
            let pool = connect(&args.database_url).await?;
            let statuses = migrate::status(&pool, &args.line)
                .await
                .context("migrate list failed")?;
            println!("line: {}", args.line);
            println!("{:<5} {:<16} {:<10} applied_at", "#", "version", "name");
            println!("{}", "-".repeat(60));
            for (i, s) in statuses.iter().enumerate() {
                let applied = s
                    .applied_at
                    .map(|t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string())
                    .unwrap_or_else(|| "— pending —".into());
                println!("{:<5} {:<16} {:<10} {}", i + 1, s.version, s.name, applied);
            }
            Ok(())
        }

        MigrateCmd::Get(args) => {
            let direction = Direction::from(args.direction);
            let sql = migrate::get_sql(args.version, direction).with_context(|| {
                format!(
                    "no migration found for version {}; `eddyq migrate list` to see known versions",
                    args.version
                )
            })?;
            print!("{sql}");
            Ok(())
        }

        MigrateCmd::Down(args) => {
            if !args.confirm {
                let statuses =
                    migrate::status(&connect(&args.database_url).await?, &args.line).await?;
                let to_drop: Vec<_> = statuses
                    .iter()
                    .rev()
                    .filter(|s| s.is_applied())
                    .take(args.max_steps)
                    .collect();
                eprintln!(
                    "DRY RUN [line={}]. Would roll back {} migration(s):",
                    args.line,
                    to_drop.len()
                );
                for s in &to_drop {
                    eprintln!("  - {}  {}", s.version, s.name);
                }
                eprintln!("\nRe-run with --confirm to execute.");
                return Ok(());
            }
            let pool = connect(&args.database_url).await?;
            let report = migrate::down(&pool, &args.line, args.max_steps)
                .await
                .context("migrate down failed")?;
            if report.rolled_back.is_empty() {
                println!("eddyq [line={}]: nothing to roll back.", args.line);
            } else {
                println!(
                    "eddyq [line={}]: rolled back {} migration(s):",
                    args.line,
                    report.rolled_back.len()
                );
                for (v, n) in &report.rolled_back {
                    println!("  - {v}  {n}");
                }
            }
            Ok(())
        }
    }
}

async fn connect(url: &str) -> Result<sqlx::PgPool> {
    PgPoolOptions::new()
        .max_connections(4)
        .connect(url)
        .await
        .with_context(|| format!("failed to connect to {url}"))
}
