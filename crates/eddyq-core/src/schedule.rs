use std::str::FromStr;

use chrono::{DateTime, Utc};
use cron::Schedule as CronSchedule;
use sqlx::PgPool;

use crate::{error::Result, job::Job};

/// Advisory lock key used by the scheduler for leader election.
/// Derived from ASCII "eddyqsch" (big-endian) so each eddyq subsystem has its own.
pub(crate) const SCHEDULER_LOCK_KEY: i64 = 0x6564_6479_7173_6368u64 as i64;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Schedule {
    pub name: String,
    pub kind: String,
    pub payload: serde_json::Value,
    pub cron_expr: String,
    pub next_run_at: DateTime<Utc>,
    pub last_run_at: Option<DateTime<Utc>>,
    pub enabled: bool,
    pub priority: i16,
    pub max_attempts: i32,
}

/// Upsert a recurring schedule. Re-calling with the same `name` updates the
/// payload / cron / priority fields but preserves `last_run_at`.
pub async fn upsert_schedule<J: Job>(
    pool: &PgPool,
    name: &str,
    cron_expr: &str,
    job: &J,
) -> Result<()> {
    let payload = serde_json::to_value(job)?;
    upsert_schedule_raw(
        pool,
        name,
        cron_expr,
        J::KIND,
        payload,
        job.priority(),
        job.max_attempts(),
    )
    .await
}

/// JSON-payload variant of `upsert_schedule` — used by language bindings and
/// dynamic callers that don't have a typed `Job` at hand. Validates the cron
/// expression and computes the first `next_run_at` before inserting.
pub async fn upsert_schedule_raw(
    pool: &PgPool,
    name: &str,
    cron_expr: &str,
    kind: &str,
    payload: serde_json::Value,
    priority: i16,
    max_attempts: i32,
) -> Result<()> {
    let schedule = CronSchedule::from_str(cron_expr)
        .map_err(|e| crate::error::Error::Cron(e.to_string()))?;
    let next = schedule
        .upcoming(Utc)
        .next()
        .ok_or_else(|| crate::error::Error::Cron("cron never fires".into()))?;

    sqlx::query(
        r#"
        INSERT INTO eddyq_schedules
            (name, kind, payload, cron_expr, next_run_at, priority, max_attempts)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        ON CONFLICT (name) DO UPDATE
            SET kind         = EXCLUDED.kind,
                payload      = EXCLUDED.payload,
                cron_expr    = EXCLUDED.cron_expr,
                next_run_at  = EXCLUDED.next_run_at,
                priority     = EXCLUDED.priority,
                max_attempts = EXCLUDED.max_attempts,
                updated_at   = NOW()
        "#,
    )
    .bind(name)
    .bind(kind)
    .bind(payload)
    .bind(cron_expr)
    .bind(next)
    .bind(priority)
    .bind(max_attempts)
    .execute(pool)
    .await?;

    Ok(())
}

pub async fn remove_schedule(pool: &PgPool, name: &str) -> Result<bool> {
    let res = sqlx::query("DELETE FROM eddyq_schedules WHERE name = $1")
        .bind(name)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

pub async fn set_enabled(pool: &PgPool, name: &str, enabled: bool) -> Result<bool> {
    let res = sqlx::query(
        "UPDATE eddyq_schedules SET enabled = $2, updated_at = NOW() WHERE name = $1",
    )
    .bind(name)
    .bind(enabled)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

pub async fn list_schedules(pool: &PgPool) -> Result<Vec<Schedule>> {
    let rows = sqlx::query_as::<_, Schedule>(
        "SELECT name, kind, payload, cron_expr, next_run_at, last_run_at, enabled, priority, max_attempts
         FROM eddyq_schedules
         ORDER BY name",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// One scheduler tick — acquires the advisory lock, enqueues jobs for all due
/// schedules, and advances `next_run_at` using skip-missed semantics (one enqueue
/// per tick regardless of how many runs were missed). Returns the number of jobs
/// enqueued.
pub(crate) async fn tick(pool: &PgPool) -> Result<usize> {
    let mut tx = pool.begin().await?;

    let got_lock: bool =
        sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1)")
            .bind(SCHEDULER_LOCK_KEY)
            .fetch_one(&mut *tx)
            .await?;
    if !got_lock {
        return Ok(0);
    }

    let due: Vec<Schedule> = sqlx::query_as::<_, Schedule>(
        "SELECT name, kind, payload, cron_expr, next_run_at, last_run_at, enabled, priority, max_attempts
         FROM eddyq_schedules
         WHERE enabled AND next_run_at <= NOW()
         FOR UPDATE",
    )
    .fetch_all(&mut *tx)
    .await?;

    let mut enqueued = 0usize;
    let mut notify = false;
    for s in due {
        sqlx::query(
            r#"
            INSERT INTO eddyq_jobs (kind, payload, state, priority, max_attempts, scheduled_at)
            VALUES ($1, $2, 'pending', $3, $4, NOW())
            "#,
        )
        .bind(&s.kind)
        .bind(&s.payload)
        .bind(s.priority)
        .bind(s.max_attempts)
        .execute(&mut *tx)
        .await?;
        enqueued += 1;
        notify = true;

        let schedule = CronSchedule::from_str(&s.cron_expr)
            .map_err(|e| crate::error::Error::Cron(e.to_string()))?;
        let next = schedule
            .upcoming(Utc)
            .next()
            .ok_or_else(|| crate::error::Error::Cron("cron never fires".into()))?;

        sqlx::query(
            "UPDATE eddyq_schedules
                SET next_run_at = $2,
                    last_run_at = NOW(),
                    updated_at  = NOW()
              WHERE name = $1",
        )
        .bind(&s.name)
        .bind(next)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;

    if notify {
        let _ = sqlx::query("SELECT pg_notify('eddyq_job', '')")
            .execute(pool)
            .await;
    }

    Ok(enqueued)
}
