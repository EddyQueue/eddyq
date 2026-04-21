//! Custom migration runner. Own tracking table (`_eddyq_migrations`) so we
//! don't collide with users' existing migration tooling (sqlx, goose, etc.).
//!
//! Migrations are embedded at compile time via `include_str!` — binaries ship
//! with their entire schema history. Each migration has an up and a down.
//!
//! The `get_sql` API lets users export the raw SQL for integration with an
//! existing migration framework, same escape hatch River's `migrate-get` offers.
//!
//! # Lines
//!
//! A *line* is a named migration sequence (default: `"main"`). Lines let
//! different logical eddyq instances track their own applied-version history
//! independently. A `--line canary` migration run records its progress
//! separately from `--line main`.
//!
//! **Lines tag tracking only.** They do NOT give you isolated tables. For true
//! isolation between two eddyq deployments sharing a Postgres database, use
//! separate databases or separate Postgres schemas (via `search_path` on your
//! connection string). Lines are a metadata label on top of that.

use sqlx::{PgPool, Postgres, Transaction};

use crate::error::Result;

/// Default line name. Matches River.
pub const DEFAULT_LINE: &str = "main";

/// A single migration, embedded at compile time.
#[derive(Debug, Clone, Copy)]
pub struct Migration {
    pub version: i64,
    pub name: &'static str,
    pub up_sql: &'static str,
    pub down_sql: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Up,
    Down,
}

/// Applied-migration record.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AppliedRow {
    pub version: i64,
    pub name: String,
    pub applied_at: chrono::DateTime<chrono::Utc>,
}

/// Per-version status — applied or pending, with direction info.
#[derive(Debug, Clone)]
pub struct MigrationStatus {
    pub version: i64,
    pub name: &'static str,
    pub applied_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl MigrationStatus {
    pub fn is_applied(&self) -> bool {
        self.applied_at.is_some()
    }
}

/// Result of a migrate-up or migrate-down call.
#[derive(Debug, Default)]
pub struct MigrateReport {
    pub applied: Vec<(i64, &'static str)>,
    pub rolled_back: Vec<(i64, &'static str)>,
}

/// All registered migrations, newest last.
pub const MIGRATIONS: &[Migration] = &[Migration {
    version: 20_260_421_000_001,
    name: "init",
    up_sql: include_str!("../migrations/20260421000001_init.up.sql"),
    down_sql: include_str!("../migrations/20260421000001_init.down.sql"),
}];

/// Fresh-install DDL for the tracking table: composite PK `(line, version)`
/// so each line has an independent applied-version history.
const TRACKING_TABLE_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS _eddyq_migrations (
    line       TEXT        NOT NULL DEFAULT 'main',
    version    BIGINT      NOT NULL,
    name       TEXT        NOT NULL,
    applied_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (line, version)
);
"#;

/// Idempotent upgrade for pre-lines tracking tables (installs created against
/// a pre-release version of eddyq). Takes a pre-lines table:
///     (version PK, name, applied_at)
/// and produces the post-lines shape:
///     (line, version, name, applied_at) PRIMARY KEY (line, version)
const TRACKING_TABLE_UPGRADE: &str = r#"
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = '_eddyq_migrations')
       AND NOT EXISTS (
           SELECT 1 FROM information_schema.columns
            WHERE table_name = '_eddyq_migrations' AND column_name = 'line'
       )
    THEN
        ALTER TABLE _eddyq_migrations ADD COLUMN line TEXT NOT NULL DEFAULT 'main';
        ALTER TABLE _eddyq_migrations DROP CONSTRAINT _eddyq_migrations_pkey;
        ALTER TABLE _eddyq_migrations ADD PRIMARY KEY (line, version);
    END IF;
END$$;
"#;

async fn ensure_tracking_table(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    sqlx::raw_sql(TRACKING_TABLE_DDL).execute(&mut **tx).await?;
    sqlx::raw_sql(TRACKING_TABLE_UPGRADE).execute(&mut **tx).await?;
    Ok(())
}

async fn ensure_tracking_table_pool(pool: &PgPool) -> Result<()> {
    sqlx::raw_sql(TRACKING_TABLE_DDL).execute(pool).await?;
    sqlx::raw_sql(TRACKING_TABLE_UPGRADE).execute(pool).await?;
    Ok(())
}

async fn list_applied(pool: &PgPool, line: &str) -> Result<Vec<AppliedRow>> {
    ensure_tracking_table_pool(pool).await?;
    let rows = sqlx::query_as::<_, AppliedRow>(
        "SELECT version, name, applied_at
           FROM _eddyq_migrations
          WHERE line = $1
       ORDER BY version",
    )
    .bind(line)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Apply all pending up-migrations for `line`. Idempotent — already-applied
/// versions are skipped. Each migration runs in its own transaction so a
/// partial-failure leaves the DB at the last-successful version, not mid-step.
pub async fn up(pool: &PgPool, line: &str) -> Result<MigrateReport> {
    let applied: std::collections::HashSet<i64> = list_applied(pool, line)
        .await?
        .into_iter()
        .map(|r| r.version)
        .collect();

    let mut report = MigrateReport::default();
    for m in MIGRATIONS {
        if applied.contains(&m.version) {
            continue;
        }
        let mut tx = pool.begin().await?;
        ensure_tracking_table(&mut tx).await?;
        // raw_sql because migration files contain multiple statements
        // (CREATE TABLE + indexes), which sqlx's prepared-statement path rejects.
        sqlx::raw_sql(m.up_sql).execute(&mut *tx).await?;
        sqlx::query(
            "INSERT INTO _eddyq_migrations (line, version, name) VALUES ($1, $2, $3)",
        )
        .bind(line)
        .bind(m.version)
        .bind(m.name)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        report.applied.push((m.version, m.name));
    }
    Ok(report)
}

/// Roll back up to `max_steps` migrations on `line`, most recent first.
pub async fn down(pool: &PgPool, line: &str, max_steps: usize) -> Result<MigrateReport> {
    let mut applied = list_applied(pool, line).await?;
    applied.sort_by_key(|r| std::cmp::Reverse(r.version));

    let mut report = MigrateReport::default();
    for row in applied.into_iter().take(max_steps) {
        let Some(m) = MIGRATIONS.iter().find(|m| m.version == row.version) else {
            continue;
        };
        let mut tx = pool.begin().await?;
        sqlx::raw_sql(m.down_sql).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM _eddyq_migrations WHERE line = $1 AND version = $2")
            .bind(line)
            .bind(m.version)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        report.rolled_back.push((m.version, m.name));
    }
    Ok(report)
}

/// Applied + pending migrations for `line`, one entry per registered version.
pub async fn status(pool: &PgPool, line: &str) -> Result<Vec<MigrationStatus>> {
    let applied: std::collections::HashMap<i64, chrono::DateTime<chrono::Utc>> =
        list_applied(pool, line)
            .await?
            .into_iter()
            .map(|r| (r.version, r.applied_at))
            .collect();

    Ok(MIGRATIONS
        .iter()
        .map(|m| MigrationStatus {
            version: m.version,
            name: m.name,
            applied_at: applied.get(&m.version).copied(),
        })
        .collect())
}

/// List all distinct lines present in `_eddyq_migrations`. Useful for
/// `eddyq migrate lines` (not yet wired) or admin tooling.
pub async fn list_lines(pool: &PgPool) -> Result<Vec<String>> {
    ensure_tracking_table_pool(pool).await?;
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT DISTINCT line FROM _eddyq_migrations ORDER BY line",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(l,)| l).collect())
}

/// Look up the raw SQL for a given version + direction. For `eddyq migrate get`
/// — users pipe this into their own migration tool (goose, atlas, etc).
pub fn get_sql(version: i64, direction: Direction) -> Option<&'static str> {
    let m = MIGRATIONS.iter().find(|m| m.version == version)?;
    Some(match direction {
        Direction::Up => m.up_sql,
        Direction::Down => m.down_sql,
    })
}
