use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::error::Result;

/// Effectively "unlimited" concurrency for groups that haven't had a cap set.
/// Matches the schema default (`i32::MAX`).
pub const UNLIMITED: i32 = i32::MAX;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Group {
    pub key: String,
    pub running_count: i32,
    pub max_concurrency: i32,
    pub paused: bool,
    pub rate_count: Option<i32>,
    pub rate_period_ms: Option<i32>,
    pub tokens: f64,
    pub tokens_refilled_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Upsert a concurrency cap for `key`. Creates the row if needed. Does not touch
/// `running_count` or `paused`.
pub async fn set_concurrency(pool: &PgPool, key: &str, max: i32) -> Result<()> {
    let max = max.max(0);
    sqlx::query(
        r#"
        INSERT INTO eddyq_groups (key, max_concurrency)
        VALUES ($1, $2)
        ON CONFLICT (key) DO UPDATE
           SET max_concurrency = EXCLUDED.max_concurrency,
               updated_at      = NOW()
        "#,
    )
    .bind(key)
    .bind(max)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_paused(pool: &PgPool, key: &str, paused: bool) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO eddyq_groups (key, paused)
        VALUES ($1, $2)
        ON CONFLICT (key) DO UPDATE
           SET paused     = EXCLUDED.paused,
               updated_at = NOW()
        "#,
    )
    .bind(key)
    .bind(paused)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get(pool: &PgPool, key: &str) -> Result<Option<Group>> {
    let row = sqlx::query_as::<_, Group>(
        "SELECT key, running_count, max_concurrency, paused,
                rate_count, rate_period_ms, tokens, tokens_refilled_at,
                created_at, updated_at
           FROM eddyq_groups
          WHERE key = $1",
    )
    .bind(key)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn list(pool: &PgPool) -> Result<Vec<Group>> {
    let rows = sqlx::query_as::<_, Group>(
        "SELECT key, running_count, max_concurrency, paused,
                rate_count, rate_period_ms, tokens, tokens_refilled_at,
                created_at, updated_at
           FROM eddyq_groups
          ORDER BY key",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Set a throughput rate limit for `key`: at most `count` jobs may be *started*
/// per `period`. Initial bucket is full. Creates the row if needed.
pub async fn set_rate(pool: &PgPool, key: &str, count: u32, period: Duration) -> Result<()> {
    let count = i32::try_from(count).unwrap_or(i32::MAX);
    let period_ms = i32::try_from(period.as_millis()).unwrap_or(i32::MAX);
    if count <= 0 || period_ms <= 0 {
        return Err(crate::error::Error::InvalidArgument(
            "rate count and period must both be positive".into(),
        ));
    }
    sqlx::query(
        r#"
        INSERT INTO eddyq_groups (key, rate_count, rate_period_ms, tokens, tokens_refilled_at)
        VALUES ($1, $2, $3, $2, NOW())
        ON CONFLICT (key) DO UPDATE
           SET rate_count         = EXCLUDED.rate_count,
               rate_period_ms     = EXCLUDED.rate_period_ms,
               tokens             = EXCLUDED.tokens,
               tokens_refilled_at = EXCLUDED.tokens_refilled_at,
               updated_at         = NOW()
        "#,
    )
    .bind(key)
    .bind(count)
    .bind(period_ms)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn clear_rate(pool: &PgPool, key: &str) -> Result<()> {
    sqlx::query(
        r#"
        UPDATE eddyq_groups
           SET rate_count         = NULL,
               rate_period_ms     = NULL,
               tokens             = 0,
               tokens_refilled_at = NULL,
               updated_at         = NOW()
         WHERE key = $1
        "#,
    )
    .bind(key)
    .execute(pool)
    .await?;
    Ok(())
}
