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
    let group_key = opts.group_key.or_else(|| job.group_key());
    let scheduled_at = opts.scheduled_at.unwrap_or_else(Utc::now);
    let due_now = scheduled_at <= Utc::now();

    let row: Option<(JobId,)> = sqlx::query_as(
        r#"
        INSERT INTO eddyq_jobs (kind, payload, state, priority, max_attempts, scheduled_at, unique_key, group_key)
        VALUES ($1, $2, 'pending', $3, $4, $5, $6, $7)
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
    .bind(group_key)
    .fetch_optional(&mut *conn)
    .await?;

    let result = match row {
        Some((id,)) => EnqueueResult::Inserted(id),
        None => EnqueueResult::Skipped,
    };

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
