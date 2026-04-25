use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool, Postgres, Transaction};

use crate::{
    error::Result,
    job::{Job, JobId},
};

#[derive(Default)]
pub struct EnqueueOptions {
    pub scheduled_at: Option<DateTime<Utc>>,
    pub max_attempts: Option<i32>,
    pub priority: Option<i16>,
    pub unique_key: Option<String>,
    pub group_key: Option<String>,
    pub tags: Option<Vec<String>>,
    pub metadata: Option<serde_json::Value>,
    pub queue: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueResult {
    Inserted(JobId),
    Skipped,
}

/// Insert the job row. Shared between the pooled and transactional entry points.
/// Returns `(result, due_now)` — callers decide whether to fire `pg_notify`.
async fn insert_job<J: Job>(
    conn: &mut PgConnection,
    job: &J,
    opts: EnqueueOptions,
) -> Result<(EnqueueResult, bool)> {
    let payload = serde_json::to_value(job)?;
    let max_attempts = opts.max_attempts.unwrap_or_else(|| job.max_attempts());
    let priority = opts.priority.unwrap_or_else(|| job.priority());
    let unique_key = opts.unique_key.or_else(|| job.unique_key());
    let group_key = opts.group_key.clone().or_else(|| job.group_key());
    let tags = opts.tags.unwrap_or_else(|| job.tags());
    let metadata = opts
        .metadata
        .or_else(|| job.metadata())
        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
    let queue = opts.queue.unwrap_or_else(|| job.queue().to_owned());
    let scheduled_at = opts.scheduled_at.unwrap_or_else(Utc::now);
    let due_now = scheduled_at <= Utc::now();

    let row: Option<(JobId,)> = sqlx::query_as(
        r#"
        INSERT INTO eddyq_jobs (kind, payload, state, priority, max_attempts, scheduled_at, unique_key, group_key, tags, metadata, queue)
        VALUES ($1, $2, 'pending', $3, $4, $5, $6, $7, $8, $9, $10)
        ON CONFLICT DO NOTHING
        RETURNING id
        "#,
    )
    .bind(J::KIND)
    .bind(payload)
    .bind(priority)
    .bind(max_attempts)
    .bind(scheduled_at)
    .bind(unique_key)
    .bind(&group_key)
    .bind(&tags)
    .bind(&metadata)
    .bind(&queue)
    .fetch_optional(&mut *conn)
    .await?;

    let result = match row {
        Some((id,)) => EnqueueResult::Inserted(id),
        None => EnqueueResult::Skipped,
    };

    // Lazily materialize an eddyq_groups row for this key from any matching
    // pattern rule. ON CONFLICT DO NOTHING means: no-op if the group already
    // has an explicit row (explicit set_group_concurrency wins), and no-op if
    // no rule matches (group stays unlimited). Only cost in the common case
    // is a single LIKE-seq-scan of eddyq_group_rules, which is typically tiny.
    if matches!(result, EnqueueResult::Inserted(_)) {
        if let Some(key) = &group_key {
            crate::group::materialize_from_rule(conn, key).await?;
        }
    }

    Ok((result, due_now))
}

/// Enqueue a job using a fresh pool connection. Best-effort NOTIFY fired
/// immediately after INSERT (no surrounding user transaction to coordinate with).
pub async fn enqueue<J: Job>(
    pool: &PgPool,
    job: &J,
    opts: EnqueueOptions,
) -> Result<EnqueueResult> {
    let mut conn = pool.acquire().await?;
    let (result, due_now) = insert_job(&mut conn, job, opts).await?;

    if matches!(result, EnqueueResult::Inserted(_)) && due_now {
        let _ = sqlx::query("SELECT pg_notify('eddyq_job', '')")
            .execute(&mut *conn)
            .await;
    }

    Ok(result)
}

/// Aggregate result of a bulk enqueue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BulkEnqueueResult {
    pub inserted: u64,
    pub skipped: u64,
}

/// Enqueue many jobs of the same kind in a single INSERT — dramatically faster
/// than calling `enqueue()` N times. Pattern rules still materialize per group
/// (one ensure-from-rule call per distinct group key in the batch).
///
/// **Atomicity:** INSERT + group-rule materialization + the workers NOTIFY all
/// commit (or roll back) together in one transaction.
///
/// Returns the aggregate count: inserted + skipped (due to unique-key
/// conflicts). For per-row results, use `enqueue` in a loop.
pub async fn enqueue_many<J: Job>(pool: &PgPool, jobs: &[J]) -> Result<BulkEnqueueResult> {
    if jobs.is_empty() {
        return Ok(BulkEnqueueResult {
            inserted: 0,
            skipped: 0,
        });
    }
    let mut tx = pool.begin().await?;
    let result = enqueue_many_in_tx(&mut tx, jobs).await?;
    tx.commit().await?;
    Ok(result)
}

/// Transactional bulk enqueue — all inserts (and the follow-up NOTIFY) only
/// land if the caller's transaction commits.
pub async fn enqueue_many_in_tx<J: Job>(
    tx: &mut Transaction<'_, Postgres>,
    jobs: &[J],
) -> Result<BulkEnqueueResult> {
    if jobs.is_empty() {
        return Ok(BulkEnqueueResult {
            inserted: 0,
            skipped: 0,
        });
    }
    let conn: &mut PgConnection = tx;
    let (result, any_due_now, distinct_groups) = insert_many(conn, jobs).await?;

    for g in &distinct_groups {
        let conn: &mut PgConnection = tx;
        crate::group::materialize_from_rule(conn, g).await?;
    }

    if any_due_now && result.inserted > 0 {
        let conn: &mut PgConnection = tx;
        sqlx::query("SELECT pg_notify('eddyq_job', '')")
            .execute(conn)
            .await?;
    }

    Ok(result)
}

async fn insert_many<J: Job>(
    conn: &mut PgConnection,
    jobs: &[J],
) -> Result<(BulkEnqueueResult, bool, Vec<String>)> {
    let now = Utc::now();
    let mut payloads: Vec<serde_json::Value> = Vec::with_capacity(jobs.len());
    let mut priorities: Vec<i16> = Vec::with_capacity(jobs.len());
    let mut max_attempts: Vec<i32> = Vec::with_capacity(jobs.len());
    let mut scheduled_ats: Vec<DateTime<Utc>> = Vec::with_capacity(jobs.len());
    let mut unique_keys: Vec<Option<String>> = Vec::with_capacity(jobs.len());
    let mut group_keys: Vec<Option<String>> = Vec::with_capacity(jobs.len());
    // Per-row text[] tags can't be passed as a Postgres multidim array
    // (those require rectangular shape). Serialize each row's tags as a JSON
    // array and convert back to text[] in SQL via `jsonb_array_elements_text`.
    let mut tags: Vec<serde_json::Value> = Vec::with_capacity(jobs.len());
    let mut metadatas: Vec<serde_json::Value> = Vec::with_capacity(jobs.len());
    let mut queues: Vec<String> = Vec::with_capacity(jobs.len());

    for job in jobs {
        payloads.push(serde_json::to_value(job)?);
        priorities.push(job.priority());
        max_attempts.push(job.max_attempts());
        scheduled_ats.push(now);
        unique_keys.push(job.unique_key());
        group_keys.push(job.group_key());
        tags.push(serde_json::Value::Array(
            job.tags()
                .into_iter()
                .map(serde_json::Value::String)
                .collect(),
        ));
        metadatas.push(
            job.metadata()
                .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new())),
        );
        queues.push(job.queue().to_owned());
    }

    let rows_inserted: (i64,) = sqlx::query_as(
        r#"
        WITH inserted AS (
            INSERT INTO eddyq_jobs (kind, payload, state, priority, max_attempts, scheduled_at, unique_key, group_key, tags, metadata, queue)
            SELECT $1,
                   t.payload,
                   'pending',
                   t.priority,
                   t.max_attempts,
                   t.scheduled_at,
                   t.unique_key,
                   t.group_key,
                   COALESCE(ARRAY(SELECT jsonb_array_elements_text(t.tags)), ARRAY[]::text[]),
                   t.metadata,
                   t.queue
              FROM UNNEST(
                  $2::jsonb[], $3::smallint[], $4::int[],
                  $5::timestamptz[], $6::text[], $7::text[], $8::jsonb[], $9::jsonb[], $10::text[]
              ) AS t(payload, priority, max_attempts, scheduled_at, unique_key, group_key, tags, metadata, queue)
            ON CONFLICT DO NOTHING
         RETURNING id
        )
        SELECT COUNT(*)::bigint FROM inserted
        "#,
    )
    .bind(J::KIND)
    .bind(&payloads)
    .bind(&priorities)
    .bind(&max_attempts)
    .bind(&scheduled_ats)
    .bind(&unique_keys)
    .bind(&group_keys)
    .bind(&tags)
    .bind(&metadatas)
    .bind(&queues)
    .fetch_one(&mut *conn)
    .await?;

    let inserted = u64::try_from(rows_inserted.0).unwrap_or(0);
    let total = jobs.len() as u64;
    let skipped = total.saturating_sub(inserted);

    let mut distinct_groups: Vec<String> = group_keys.into_iter().flatten().collect();
    distinct_groups.sort();
    distinct_groups.dedup();

    Ok((
        BulkEnqueueResult { inserted, skipped },
        true,
        distinct_groups,
    ))
}

/// Dynamic-kind enqueue request. Used by bindings (Node, CLI, future SDKs)
/// that don't have a compile-time `Job` trait implementation. All fields are
/// explicit — the caller provides the defaults a `Job` impl would normally
/// supply.
#[derive(Debug, Clone)]
pub struct DynEnqueue {
    pub kind: String,
    pub payload: serde_json::Value,
    pub max_attempts: i32,
    pub priority: i16,
    pub queue: String,
    pub scheduled_at: Option<DateTime<Utc>>,
    pub unique_key: Option<String>,
    pub group_key: Option<String>,
    pub tags: Vec<String>,
    pub metadata: serde_json::Value,
}

impl DynEnqueue {
    /// Build a request with the same defaults the `Job` trait provides:
    /// `max_attempts=3`, `priority=0`, `queue="default"`, empty tags/metadata.
    pub fn new(kind: impl Into<String>, payload: serde_json::Value) -> Self {
        Self {
            kind: kind.into(),
            payload,
            max_attempts: 3,
            priority: 0,
            queue: crate::job::DEFAULT_QUEUE.to_owned(),
            scheduled_at: None,
            unique_key: None,
            group_key: None,
            tags: Vec::new(),
            metadata: serde_json::Value::Object(serde_json::Map::new()),
        }
    }
}

async fn insert_dyn(conn: &mut PgConnection, req: DynEnqueue) -> Result<(EnqueueResult, bool)> {
    let scheduled_at = req.scheduled_at.unwrap_or_else(Utc::now);
    let due_now = scheduled_at <= Utc::now();

    let row: Option<(JobId,)> = sqlx::query_as(
        r#"
        INSERT INTO eddyq_jobs (kind, payload, state, priority, max_attempts, scheduled_at, unique_key, group_key, tags, metadata, queue)
        VALUES ($1, $2, 'pending', $3, $4, $5, $6, $7, $8, $9, $10)
        ON CONFLICT DO NOTHING
        RETURNING id
        "#,
    )
    .bind(&req.kind)
    .bind(&req.payload)
    .bind(req.priority)
    .bind(req.max_attempts)
    .bind(scheduled_at)
    .bind(&req.unique_key)
    .bind(&req.group_key)
    .bind(&req.tags)
    .bind(&req.metadata)
    .bind(&req.queue)
    .fetch_optional(&mut *conn)
    .await?;

    let result = match row {
        Some((id,)) => EnqueueResult::Inserted(id),
        None => EnqueueResult::Skipped,
    };

    if matches!(result, EnqueueResult::Inserted(_)) {
        if let Some(key) = &req.group_key {
            crate::group::materialize_from_rule(conn, key).await?;
        }
    }

    Ok((result, due_now))
}

/// Pool-based dynamic enqueue. Mirrors `enqueue` but drops the generic `Job`
/// bound so callers like the Node bindings can pass `kind` and `payload` directly.
pub async fn enqueue_dyn(pool: &PgPool, req: DynEnqueue) -> Result<EnqueueResult> {
    let mut conn = pool.acquire().await?;
    let (result, due_now) = insert_dyn(&mut conn, req).await?;

    if matches!(result, EnqueueResult::Inserted(_)) && due_now {
        let _ = sqlx::query("SELECT pg_notify('eddyq_job', '')")
            .execute(&mut *conn)
            .await;
    }

    Ok(result)
}

/// Transactional dynamic enqueue — atomic with the caller's transaction.
pub async fn enqueue_dyn_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    req: DynEnqueue,
) -> Result<EnqueueResult> {
    let conn: &mut PgConnection = tx;
    let (result, due_now) = insert_dyn(conn, req).await?;

    if matches!(result, EnqueueResult::Inserted(_)) && due_now {
        let conn: &mut PgConnection = tx;
        sqlx::query("SELECT pg_notify('eddyq_job', '')")
            .execute(conn)
            .await?;
    }

    Ok(result)
}

async fn insert_many_dyn(
    conn: &mut PgConnection,
    reqs: Vec<DynEnqueue>,
) -> Result<(BulkEnqueueResult, bool, Vec<String>)> {
    let now = Utc::now();
    let n = reqs.len();
    let mut kinds: Vec<String> = Vec::with_capacity(n);
    let mut payloads: Vec<serde_json::Value> = Vec::with_capacity(n);
    let mut priorities: Vec<i16> = Vec::with_capacity(n);
    let mut max_attempts: Vec<i32> = Vec::with_capacity(n);
    let mut scheduled_ats: Vec<DateTime<Utc>> = Vec::with_capacity(n);
    let mut unique_keys: Vec<Option<String>> = Vec::with_capacity(n);
    let mut group_keys: Vec<Option<String>> = Vec::with_capacity(n);
    // Per-row `text[]` tags can't be passed as a Postgres multidim array
    // (those require rectangular shape). Serialize each row's tags as a JSON
    // array and convert back to `text[]` in SQL via `jsonb_array_elements_text`.
    let mut tags: Vec<serde_json::Value> = Vec::with_capacity(n);
    let mut metadatas: Vec<serde_json::Value> = Vec::with_capacity(n);
    let mut queues: Vec<String> = Vec::with_capacity(n);

    let mut any_due_now = false;
    for req in reqs {
        let scheduled_at = req.scheduled_at.unwrap_or(now);
        if scheduled_at <= now {
            any_due_now = true;
        }
        kinds.push(req.kind);
        payloads.push(req.payload);
        priorities.push(req.priority);
        max_attempts.push(req.max_attempts);
        scheduled_ats.push(scheduled_at);
        unique_keys.push(req.unique_key);
        group_keys.push(req.group_key);
        tags.push(serde_json::Value::Array(
            req.tags
                .into_iter()
                .map(serde_json::Value::String)
                .collect(),
        ));
        metadatas.push(req.metadata);
        queues.push(req.queue);
    }

    let rows_inserted: (i64,) = sqlx::query_as(
        r#"
        WITH inserted AS (
            INSERT INTO eddyq_jobs (kind, payload, state, priority, max_attempts, scheduled_at, unique_key, group_key, tags, metadata, queue)
            SELECT t.kind,
                   t.payload,
                   'pending',
                   t.priority,
                   t.max_attempts,
                   t.scheduled_at,
                   t.unique_key,
                   t.group_key,
                   COALESCE(ARRAY(SELECT jsonb_array_elements_text(t.tags)), ARRAY[]::text[]),
                   t.metadata,
                   t.queue
              FROM UNNEST(
                  $1::text[], $2::jsonb[], $3::smallint[], $4::int[],
                  $5::timestamptz[], $6::text[], $7::text[], $8::jsonb[], $9::jsonb[], $10::text[]
              ) AS t(kind, payload, priority, max_attempts, scheduled_at, unique_key, group_key, tags, metadata, queue)
            ON CONFLICT DO NOTHING
         RETURNING id
        )
        SELECT COUNT(*)::bigint FROM inserted
        "#,
    )
    .bind(&kinds)
    .bind(&payloads)
    .bind(&priorities)
    .bind(&max_attempts)
    .bind(&scheduled_ats)
    .bind(&unique_keys)
    .bind(&group_keys)
    .bind(&tags)
    .bind(&metadatas)
    .bind(&queues)
    .fetch_one(&mut *conn)
    .await?;

    let inserted = u64::try_from(rows_inserted.0).unwrap_or(0);
    let total = n as u64;
    let skipped = total.saturating_sub(inserted);

    let mut distinct_groups: Vec<String> = group_keys.into_iter().flatten().collect();
    distinct_groups.sort();
    distinct_groups.dedup();

    Ok((
        BulkEnqueueResult { inserted, skipped },
        any_due_now,
        distinct_groups,
    ))
}

/// Dynamic-kind bulk enqueue. Mixed kinds in one batch are supported — unlike
/// the typed `enqueue_many`, which constrains the batch to a single `J::KIND`.
/// Per-item `tags` are passed through.
///
/// **Atomicity:** INSERT + group-rule materialization + the workers NOTIFY all
/// commit (or roll back) together in one transaction.
pub async fn enqueue_many_dyn(pool: &PgPool, reqs: Vec<DynEnqueue>) -> Result<BulkEnqueueResult> {
    if reqs.is_empty() {
        return Ok(BulkEnqueueResult {
            inserted: 0,
            skipped: 0,
        });
    }
    let mut tx = pool.begin().await?;
    let result = enqueue_many_dyn_in_tx(&mut tx, reqs).await?;
    tx.commit().await?;
    Ok(result)
}

/// Transactional dynamic bulk enqueue — atomic with the caller's transaction.
pub async fn enqueue_many_dyn_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    reqs: Vec<DynEnqueue>,
) -> Result<BulkEnqueueResult> {
    if reqs.is_empty() {
        return Ok(BulkEnqueueResult {
            inserted: 0,
            skipped: 0,
        });
    }
    let conn: &mut PgConnection = tx;
    let (result, any_due_now, distinct_groups) = insert_many_dyn(conn, reqs).await?;

    for g in &distinct_groups {
        let conn: &mut PgConnection = tx;
        crate::group::materialize_from_rule(conn, g).await?;
    }

    if any_due_now && result.inserted > 0 {
        let conn: &mut PgConnection = tx;
        sqlx::query("SELECT pg_notify('eddyq_job', '')")
            .execute(conn)
            .await?;
    }

    Ok(result)
}

/// Enqueue a job *inside the caller's transaction*. The row — and the NOTIFY —
/// are visible only if the user commits. Rolling back the transaction discards
/// both. This is the defining correctness guarantee of eddyq vs Redis-backed
/// queues: your business write and the follow-up job commit atomically.
///
/// ```ignore
/// let mut tx = pool.begin().await?;
/// db::save_invoice(&mut tx, &invoice).await?;
/// queue.enqueue_in_tx(&mut tx, &SendReceipt { invoice_id: invoice.id }).await?;
/// tx.commit().await?;  // — both or neither.
/// ```
pub async fn enqueue_in_tx<J: Job>(
    tx: &mut Transaction<'_, Postgres>,
    job: &J,
    opts: EnqueueOptions,
) -> Result<EnqueueResult> {
    let conn: &mut PgConnection = tx;
    let (result, due_now) = insert_job(conn, job, opts).await?;

    if matches!(result, EnqueueResult::Inserted(_)) && due_now {
        // This pg_notify is buffered by Postgres and only actually delivered at
        // the user's COMMIT. If they roll back, no NOTIFY fires — the job row is
        // gone too, so that's correct.
        let conn: &mut PgConnection = tx;
        sqlx::query("SELECT pg_notify('eddyq_job', '')")
            .execute(conn)
            .await?;
    }

    Ok(result)
}
