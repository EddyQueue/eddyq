//! Read-only queries for dashboards / admin UIs.
//!
//! These don't claim, mutate, or lock anything — they're the surface a future
//! eddyq-board calls to render counts, drill down on a queue, or inspect a
//! specific job. Writes and the runtime live in other modules.

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, QueryBuilder};

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

/// Count jobs grouped by (queue, state). One round trip, suitable as the
/// landing query for a board.
pub async fn get_stats(pool: &PgPool) -> Result<JobStats> {
    let rows: Vec<(String, String, i64)> = sqlx::query_as(
        r#"
        SELECT queue, state, COUNT(*)::bigint
          FROM eddyq_jobs
         GROUP BY queue, state
         ORDER BY queue, state
        "#,
    )
    .fetch_all(pool)
    .await?;

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
}
