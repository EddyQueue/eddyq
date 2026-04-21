//! Per-queue concurrency tracking (cross-process). Tracks `running_count` for
//! each named queue so a cap like "integrations queue at 10 total" applies
//! across every replica, not per-process.
//!
//! Mechanism mirrors `eddyq_groups`: the claim query locks + reads + atomically
//! bumps `running_count`; `mark_completed` / `mark_failed` / `sweep_stale` all
//! decrement. A queue with no row in `eddyq_queues` is implicitly unlimited.

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::error::Result;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct NamedQueue {
    pub name: String,
    pub running_count: i32,
    pub max_concurrency: i32,
    pub paused: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Set (or update) the cross-process concurrency cap for `name`. Creates the
/// row if needed, initializing `running_count` from the *live* count of
/// currently-running jobs on that queue (so the cap is honest from the moment
/// you set it, even if jobs were already running unlimited before).
pub async fn set_concurrency(pool: &PgPool, name: &str, max: i32) -> Result<()> {
    let max = max.max(0);
    sqlx::query(
        r#"
        INSERT INTO eddyq_queues (name, max_concurrency, running_count)
        SELECT $1, $2, COUNT(*)
          FROM eddyq_jobs
         WHERE queue = $1 AND state = 'running'
        ON CONFLICT (name) DO UPDATE
           SET max_concurrency = EXCLUDED.max_concurrency,
               updated_at      = NOW()
        "#,
    )
    .bind(name)
    .bind(max)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_paused(pool: &PgPool, name: &str, paused: bool) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO eddyq_queues (name, paused, running_count)
        SELECT $1, $2, COUNT(*)
          FROM eddyq_jobs
         WHERE queue = $1 AND state = 'running'
        ON CONFLICT (name) DO UPDATE
           SET paused     = EXCLUDED.paused,
               updated_at = NOW()
        "#,
    )
    .bind(name)
    .bind(paused)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get(pool: &PgPool, name: &str) -> Result<Option<NamedQueue>> {
    let row = sqlx::query_as::<_, NamedQueue>(
        "SELECT name, running_count, max_concurrency, paused, created_at, updated_at
           FROM eddyq_queues
          WHERE name = $1",
    )
    .bind(name)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn list(pool: &PgPool) -> Result<Vec<NamedQueue>> {
    let rows = sqlx::query_as::<_, NamedQueue>(
        "SELECT name, running_count, max_concurrency, paused, created_at, updated_at
           FROM eddyq_queues
          ORDER BY name",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}
