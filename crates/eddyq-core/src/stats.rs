//! Read-only queries for dashboards / admin UIs.
//!
//! These don't claim, mutate, or lock anything — they're the surface a future
//! eddyq-board calls to render counts, drill down on a queue, or inspect a
//! specific job. Writes and the runtime live in other modules.

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, QueryBuilder, Transaction};

use crate::{
    error::Result,
    job::{JobId, JobState},
};

/// One row of the (queue, state) → count histogram.
#[derive(Debug, Clone)]
pub struct QueueStateCount {
    pub queue: String,
    pub state: JobState,
    pub count: i64,
}

/// Snapshot of job counts grouped by (queue, state). Produced by a single
/// `GROUP BY` query — adequate for a dashboard landing page.
#[derive(Debug, Clone, Default)]
pub struct JobStats {
    pub by_queue_state: Vec<QueueStateCount>,
}

/// How stale the terminal (completed / failed / cancelled) counts in
/// `get_stats` may be. Live counts are always exact.
pub const TERMINAL_STATS_TTL_SECS: i64 = 60;

/// Count jobs grouped by (queue, state), ordered by (queue, state).
///
/// Live states (pending, running) are counted exactly on every call from the
/// partial indexes the runtime already maintains, so they cost about a
/// millisecond however large the table is. Terminal states are retained
/// history and dominate the table, so their counts come from
/// `eddyq_stats_cache` and may be up to [`TERMINAL_STATS_TTL_SECS`] old: one
/// caller across the cluster recomputes them per TTL, and every other caller
/// reads the cache. Polling this from many processes or dashboard tabs no
/// longer multiplies a full-table scan.
pub async fn get_stats(pool: &PgPool) -> Result<JobStats> {
    let mut rows: Vec<(String, String, i64)> = sqlx::query_as(
        r#"
        SELECT queue, 'pending'::text, COUNT(*)::bigint
          FROM eddyq_jobs WHERE state = 'pending' GROUP BY queue
        UNION ALL
        SELECT queue, 'running'::text, COUNT(*)::bigint
          FROM eddyq_jobs WHERE state = 'running' GROUP BY queue
        "#,
    )
    .fetch_all(pool)
    .await?;
    rows.extend(terminal_counts(pool).await?);
    rows.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));

    let by_queue_state = rows
        .into_iter()
        .filter_map(|(queue, state, count)| {
            parse_state(&state).map(|state| QueueStateCount {
                queue,
                state,
                count,
            })
        })
        .collect();

    Ok(JobStats { by_queue_state })
}

/// Terminal-state counts from `eddyq_stats_cache`, refreshing it first when
/// it is older than the TTL and this caller wins the refresh lock. A caller
/// that loses the lock returns the previous snapshot rather than waiting,
/// except on the very first call, when there is no snapshot to return.
async fn terminal_counts(pool: &PgPool) -> Result<Vec<(String, String, i64)>> {
    let mut tx = pool.begin().await?;
    // None: no snapshot yet. Some(false): older than the TTL. Judged by the
    // database clock, the one that stamped computed_at.
    let fresh = snapshot_fresh(&mut tx).await?;
    if fresh != Some(true) {
        let locked: bool = if fresh.is_none() {
            sqlx::query("SELECT pg_advisory_xact_lock($1)")
                .bind(STATS_LOCK_KEY)
                .execute(&mut *tx)
                .await?;
            true
        } else {
            sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1)")
                .bind(STATS_LOCK_KEY)
                .fetch_one(&mut *tx)
                .await?
        };
        // Another caller may have refreshed between the read and the lock;
        // read committed sees its commit, so check again before scanning.
        if locked && snapshot_fresh(&mut tx).await? != Some(true) {
            sqlx::query("DELETE FROM eddyq_stats_cache")
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                r#"
                INSERT INTO eddyq_stats_cache (queue, state, count)
                SELECT queue, state, COUNT(*)::bigint
                  FROM eddyq_jobs
                 WHERE state IN ('completed', 'failed', 'cancelled')
                 GROUP BY queue, state
                "#,
            )
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "INSERT INTO eddyq_stats_meta (id, computed_at) VALUES (1, now()) \
                 ON CONFLICT (id) DO UPDATE SET computed_at = EXCLUDED.computed_at",
            )
            .execute(&mut *tx)
            .await?;
        }
    }
    let rows = sqlx::query_as("SELECT queue, state, count FROM eddyq_stats_cache")
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(rows)
}

async fn snapshot_fresh(tx: &mut Transaction<'_, Postgres>) -> Result<Option<bool>> {
    Ok(sqlx::query_scalar(
        "SELECT computed_at > clock_timestamp() - make_interval(secs => $1) \
           FROM eddyq_stats_meta WHERE id = 1",
    )
    .bind(TERMINAL_STATS_TTL_SECS as f64)
    .fetch_optional(&mut **tx)
    .await?)
}

/// Advisory-lock key serializing terminal-count refreshes: FNV-1a 64-bit of
/// `"eddyq::stats"`, the same scheme `migrate` uses for its own key.
pub const STATS_LOCK_KEY: i64 = {
    let bytes = b"eddyq::stats";
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut i = 0;
    while i < bytes.len() {
        hash ^= bytes[i] as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
        i += 1;
    }
    hash as i64
};

fn parse_state(s: &str) -> Option<JobState> {
    match s {
        "pending" => Some(JobState::Pending),
        "running" => Some(JobState::Running),
        "completed" => Some(JobState::Completed),
        "failed" => Some(JobState::Failed),
        "scheduled" => Some(JobState::Scheduled),
        "cancelled" => Some(JobState::Cancelled),
        _ => None,
    }
}

/// Filters for `list_jobs`. All fields optional — unset = no constraint. The
/// active filters AND together.
#[derive(Debug, Clone, Default)]
pub struct ListJobsFilter {
    pub queue: Option<String>,
    pub state: Option<JobState>,
    pub kind: Option<String>,
    pub group_key: Option<String>,
    /// Jobs whose `tags` array contains this tag (`tags @> ARRAY[tag]`).
    pub tag: Option<String>,
    pub id: Option<JobId>,
    /// Keyset cursor for newest-first pagination: return only jobs with
    /// `created_at < before_created_at`. Pair with `Pagination.limit` and
    /// `offset = 0` to scroll through results in chronological pages without
    /// drift from concurrent inserts. Rows with `created_at` exactly equal
    /// to the cursor are excluded (microsecond resolution makes collisions
    /// rare; an admin UI tolerates the edge case).
    pub before_created_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy)]
pub struct Pagination {
    pub limit: i64,
    pub offset: i64,
}

impl Default for Pagination {
    fn default() -> Self {
        Self {
            limit: 50,
            offset: 0,
        }
    }
}

/// A single row returned from `list_jobs`. Mirrors the columns a dashboard
/// would render: identifying fields, lifecycle timestamps, payload + result,
/// the accumulated `errors` JSON array, and metadata.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct JobRow {
    pub id: JobId,
    pub queue: String,
    pub kind: String,
    pub state: String,
    pub priority: i16,
    pub attempt: i32,
    pub max_attempts: i32,
    pub scheduled_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub finalized_at: Option<DateTime<Utc>>,
    pub group_key: Option<String>,
    pub tags: Vec<String>,
    pub payload: serde_json::Value,
    pub result: Option<serde_json::Value>,
    pub errors: serde_json::Value,
    pub metadata: serde_json::Value,
}

#[derive(Debug, Clone, Default)]
pub struct JobList {
    /// Total rows matching the filter (ignoring limit/offset) — feeds pagination UI.
    pub total: i64,
    pub rows: Vec<JobRow>,
}

/// Cap on `limit` — keeps a dashboard from asking for 100k rows.
const MAX_LIMIT: i64 = 500;

/// Paginated listing with optional filters. Returns `(total_matching, page_rows)`.
/// Ordered newest-first by `id DESC`.
pub async fn list_jobs(
    pool: &PgPool,
    filter: ListJobsFilter,
    pagination: Pagination,
) -> Result<JobList> {
    let limit = pagination.limit.clamp(1, MAX_LIMIT);
    let offset = pagination.offset.max(0);
    let state_str = filter.state.map(|s| s.as_str().to_owned());

    // Total count (same filter, no pagination) — separate query so the page
    // rows can still be fetched by LIMIT/OFFSET without a window function.
    let mut cq: QueryBuilder<Postgres> =
        QueryBuilder::new("SELECT COUNT(*)::bigint FROM eddyq_jobs WHERE TRUE");
    push_filters(&mut cq, &filter, state_str.as_deref());
    let total: i64 = cq.build_query_scalar().fetch_one(pool).await?;

    let mut q: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT id, queue, kind, state, priority, attempt, max_attempts, \
                scheduled_at, created_at, finalized_at, group_key, tags, \
                payload, result, errors, metadata \
           FROM eddyq_jobs WHERE TRUE",
    );
    push_filters(&mut q, &filter, state_str.as_deref());
    q.push(" ORDER BY id DESC LIMIT ");
    q.push_bind(limit);
    q.push(" OFFSET ");
    q.push_bind(offset);

    let rows: Vec<JobRow> = q.build_query_as().fetch_all(pool).await?;

    Ok(JobList { total, rows })
}

fn push_filters<'a>(
    qb: &mut QueryBuilder<'a, Postgres>,
    filter: &'a ListJobsFilter,
    state_str: Option<&'a str>,
) {
    if let Some(id) = filter.id {
        qb.push(" AND id = ").push_bind(id);
    }
    if let Some(queue) = &filter.queue {
        qb.push(" AND queue = ").push_bind(queue.as_str());
    }
    if let Some(state) = state_str {
        qb.push(" AND state = ").push_bind(state);
    }
    if let Some(kind) = &filter.kind {
        qb.push(" AND kind = ").push_bind(kind.as_str());
    }
    if let Some(group) = &filter.group_key {
        qb.push(" AND group_key = ").push_bind(group.as_str());
    }
    if let Some(tag) = &filter.tag {
        // `tags @> ARRAY[$n]::text[]` uses the GIN index on eddyq_jobs.tags.
        qb.push(" AND tags @> ARRAY[")
            .push_bind(tag.as_str())
            .push("]::text[]");
    }
    if let Some(before) = filter.before_created_at {
        qb.push(" AND created_at < ").push_bind(before);
    }
}
