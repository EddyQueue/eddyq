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

// --- Pattern-based rules ----------------------------------------------------

/// A default-values rule for groups whose keys match a glob pattern. Applied
/// lazily on the first `enqueue()` for a matching group key that doesn't yet
/// have an explicit `eddyq_groups` row.
///
/// Patterns support `*` (any characters) and `?` (one character); they're
/// translated to SQL LIKE under the hood.
///
/// Rule precedence when multiple match a key: higher `priority` wins; ties
/// broken by pattern length (more specific = longer).
#[derive(Debug, Clone)]
pub struct GroupRule {
    pub max_concurrency: Option<i32>,
    pub rate_count: Option<u32>,
    pub rate_period: Option<Duration>,
    pub priority: i32,
}

impl GroupRule {
    /// Cap concurrency only; no rate limit.
    #[must_use]
    pub fn concurrency(max: i32) -> Self {
        Self {
            max_concurrency: Some(max.max(0)),
            rate_count: None,
            rate_period: None,
            priority: 0,
        }
    }

    /// Rate limit only; no concurrency cap.
    #[must_use]
    pub fn rate(count: u32, period: Duration) -> Self {
        Self {
            max_concurrency: None,
            rate_count: Some(count),
            rate_period: Some(period),
            priority: 0,
        }
    }

    /// Both: concurrency cap AND rate limit.
    #[must_use]
    pub fn both(max: i32, count: u32, period: Duration) -> Self {
        Self {
            max_concurrency: Some(max.max(0)),
            rate_count: Some(count),
            rate_period: Some(period),
            priority: 0,
        }
    }

    /// Bump priority — higher wins when multiple patterns match a key.
    #[must_use]
    pub fn with_priority(mut self, p: i32) -> Self {
        self.priority = p;
        self
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct StoredRule {
    pub pattern: String,
    pub max_concurrency: Option<i32>,
    pub rate_count: Option<i32>,
    pub rate_period_ms: Option<i32>,
    pub priority: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub async fn set_rule(pool: &PgPool, pattern: &str, rule: GroupRule) -> Result<()> {
    let period_ms = rule
        .rate_period
        .map(|p| i32::try_from(p.as_millis()).unwrap_or(i32::MAX));
    let rate_count = rule
        .rate_count
        .map(|c| i32::try_from(c).unwrap_or(i32::MAX));

    if rule.max_concurrency.is_none() && rate_count.is_none() {
        return Err(crate::error::Error::InvalidArgument(
            "GroupRule must specify at least one of max_concurrency or rate".into(),
        ));
    }

    sqlx::query(
        r#"
        INSERT INTO eddyq_group_rules (pattern, max_concurrency, rate_count, rate_period_ms, priority)
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (pattern) DO UPDATE
           SET max_concurrency = EXCLUDED.max_concurrency,
               rate_count      = EXCLUDED.rate_count,
               rate_period_ms  = EXCLUDED.rate_period_ms,
               priority        = EXCLUDED.priority,
               updated_at      = NOW()
        "#,
    )
    .bind(pattern)
    .bind(rule.max_concurrency)
    .bind(rate_count)
    .bind(period_ms)
    .bind(rule.priority)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn remove_rule(pool: &PgPool, pattern: &str) -> Result<bool> {
    let res = sqlx::query("DELETE FROM eddyq_group_rules WHERE pattern = $1")
        .bind(pattern)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

pub async fn list_rules(pool: &PgPool) -> Result<Vec<StoredRule>> {
    let rows = sqlx::query_as::<_, StoredRule>(
        "SELECT pattern, max_concurrency, rate_count, rate_period_ms, priority, created_at, updated_at
           FROM eddyq_group_rules
          ORDER BY priority DESC, LENGTH(pattern) DESC, pattern",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Try to materialize an eddyq_groups row for `key` using the best-matching
/// rule. No-op if the row already exists (ON CONFLICT DO NOTHING) or no rule
/// matches. Called by the enqueue path on any non-None group_key.
pub(crate) async fn materialize_from_rule(
    executor: &mut sqlx::PgConnection,
    key: &str,
) -> Result<()> {
    // `*` → `%`, `?` → `_`; matches SQL LIKE semantics.
    sqlx::query(
        r#"
        INSERT INTO eddyq_groups (key, max_concurrency, paused, rate_count, rate_period_ms, tokens, tokens_refilled_at)
        SELECT
            $1,
            COALESCE(r.max_concurrency, 2147483647),
            FALSE,
            r.rate_count,
            r.rate_period_ms,
            COALESCE(r.rate_count::double precision, 0),
            CASE WHEN r.rate_count IS NOT NULL THEN NOW() END
          FROM eddyq_group_rules r
         WHERE $1 LIKE REPLACE(REPLACE(r.pattern, '*', '%'), '?', '_')
      ORDER BY r.priority DESC, LENGTH(r.pattern) DESC
         LIMIT 1
        ON CONFLICT (key) DO NOTHING
        "#,
    )
    .bind(key)
    .execute(executor)
    .await?;
    Ok(())
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
