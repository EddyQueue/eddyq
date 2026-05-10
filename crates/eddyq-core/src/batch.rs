//! Native batch primitive — fan-in support.
//!
//! `enqueue_batch` groups N jobs and an optional `on_complete` callback. When
//! every job in the batch reaches a terminal state (completed, failed, or
//! cancelled), the callback fires exactly once with batch counts merged into
//! its payload under a `_eddyq_batch` envelope. Counter increments and the
//! callback claim happen in the same transaction as the underlying job-state
//! transition (see `fetch::mark_completed` / `mark_failed` / `sweep_stale`),
//! so they're race-safe across concurrent workers.

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool, Postgres, Transaction};

use crate::{
    enqueue::{DynEnqueue, enqueue_dyn_in_tx, enqueue_many_dyn_in_tx},
    error::Result,
};

/// Which counter to increment in `settle_terminal`.
#[derive(Debug, Clone, Copy)]
pub(crate) enum TerminalOutcome {
    Completed,
    Failed,
    Cancelled,
}

/// Configuration for a batch.
#[derive(Default, Debug, Clone)]
pub struct BatchOptions {
    /// Job to enqueue when every item in the batch reaches a terminal state.
    /// Fires exactly once regardless of mix of success/failure/cancellation;
    /// the handler receives counts in `_eddyq_batch` and branches as needed.
    pub on_complete: Option<DynEnqueue>,
    pub metadata: serde_json::Value,
}

/// Aggregate result of `enqueue_batch`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchEnqueueResult {
    pub batch_id: i64,
    pub inserted: u64,
    pub skipped: u64,
}

/// Snapshot of a batch's terminal counts at callback time.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BatchSettleSummary {
    pub total: i64,
    pub completed: i64,
    pub failed: i64,
    pub cancelled: i64,
    pub duration_ms: i64,
}

/// Enqueue a batch using a fresh pool transaction.
pub async fn enqueue_batch(
    pool: &PgPool,
    items: Vec<DynEnqueue>,
    opts: BatchOptions,
) -> Result<BatchEnqueueResult> {
    let mut tx = pool.begin().await?;
    let result = enqueue_batch_in_tx(&mut tx, items, opts).await?;
    tx.commit().await?;
    Ok(result)
}

/// Transactional batch enqueue — atomic with the caller's transaction. The
/// batch row, the underlying jobs, and (in the all-skipped fast path) the
/// callback enqueue all commit or roll back together.
pub async fn enqueue_batch_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    items: Vec<DynEnqueue>,
    opts: BatchOptions,
) -> Result<BatchEnqueueResult> {
    let n_requested = items.len() as u64;
    let on_complete_json = match &opts.on_complete {
        Some(spec) => Some(serde_json::to_value(spec)?),
        None => None,
    };
    let metadata = if opts.metadata.is_null() {
        serde_json::Value::Object(serde_json::Map::new())
    } else {
        opts.metadata.clone()
    };

    let conn: &mut PgConnection = tx;
    let (batch_id,): (i64,) = sqlx::query_as(
        r#"
        INSERT INTO eddyq_batches (total, on_complete, metadata)
        VALUES (0, $1, $2)
        RETURNING id
        "#,
    )
    .bind(&on_complete_json)
    .bind(&metadata)
    .fetch_one(conn)
    .await?;

    let inserted = if items.is_empty() {
        0u64
    } else {
        let stamped: Vec<DynEnqueue> = items
            .into_iter()
            .map(|mut item| {
                item.batch_id = Some(batch_id);
                item
            })
            .collect();
        let bulk = enqueue_many_dyn_in_tx(tx, stamped).await?;
        bulk.inserted
    };
    let skipped = n_requested.saturating_sub(inserted);

    // Update total to the actually-inserted count. Skipped items belong to the
    // batch that originally enqueued them — they don't count here.
    let conn: &mut PgConnection = tx;
    sqlx::query("UPDATE eddyq_batches SET total = $1 WHERE id = $2")
        .bind(inserted as i64)
        .bind(batch_id)
        .execute(conn)
        .await?;

    // Empty / all-skipped: fire the callback immediately. Holds the contract
    // that every batch reaches `state='complete'` and every `on_complete`
    // registered fires exactly once.
    if inserted == 0 {
        let conn: &mut PgConnection = tx;
        sqlx::query(
            r#"
            UPDATE eddyq_batches
               SET state = 'complete',
                   callback_enqueued_at = NOW(),
                   finalized_at = NOW()
             WHERE id = $1
            "#,
        )
        .bind(batch_id)
        .execute(conn)
        .await?;

        if let Some(spec) = opts.on_complete {
            let callback = build_callback_dyn(
                spec,
                batch_id,
                BatchSettleSummary {
                    total: 0,
                    completed: 0,
                    failed: 0,
                    cancelled: 0,
                    duration_ms: 0,
                },
            );
            enqueue_dyn_in_tx(tx, callback).await?;
        }
    }

    Ok(BatchEnqueueResult {
        batch_id,
        inserted,
        skipped,
    })
}

/// Settle one job's terminal transition into its batch's counters and, if the
/// batch is now fully terminal, atomically claim and fire the `on_complete`
/// callback. Caller is the same tx that mutated `eddyq_jobs`, so the counter
/// update commits or rolls back with the job-state change.
///
/// Race-safety: two statements inside the caller's tx. Stmt 1 (bump counters)
/// takes a row-level write lock on the batch row that holds until tx commit;
/// concurrent settlers on the same batch serialize on it. Stmt 2 (claim) uses
/// `WHERE callback_enqueued_at IS NULL` so only one tx wins. The deterministic
/// `unique_key` on the callback job is a fallback against any escape window.
pub(crate) async fn settle_terminal(
    tx: &mut Transaction<'_, Postgres>,
    batch_id: i64,
    outcome: TerminalOutcome,
) -> Result<()> {
    let (delta_completed, delta_failed, delta_cancelled) = match outcome {
        TerminalOutcome::Completed => (1i32, 0i32, 0i32),
        TerminalOutcome::Failed => (0, 1, 0),
        TerminalOutcome::Cancelled => (0, 0, 1),
    };

    let conn: &mut PgConnection = tx;
    let row: Option<(i32, i32, i32, i32, Option<DateTime<Utc>>)> = sqlx::query_as(
        r#"
        UPDATE eddyq_batches
           SET completed = completed + $2,
               failed    = failed    + $3,
               cancelled = cancelled + $4
         WHERE id = $1
     RETURNING total, completed, failed, cancelled, callback_enqueued_at
        "#,
    )
    .bind(batch_id)
    .bind(delta_completed)
    .bind(delta_failed)
    .bind(delta_cancelled)
    .fetch_optional(conn)
    .await?;

    let Some((total, completed, failed, cancelled, prev_claim)) = row else {
        return Ok(()); // batch row was deleted; nothing to do
    };
    if prev_claim.is_some() {
        return Ok(()); // already claimed by an earlier path (e.g. all-skipped)
    }
    if completed + failed + cancelled != total {
        return Ok(()); // not done yet
    }

    try_claim_and_fire(tx, batch_id).await
}

/// Attempt to atomically claim the callback slot for a batch and, if the claim
/// succeeds and the batch had a callback configured, enqueue it in the same
/// transaction. Idempotent — safe to call multiple times; only the first
/// caller (under the row lock) succeeds. Used both by `settle_terminal` (after
/// it has bumped counters) and by `sweep_stale` (where the counter bump happens
/// inside the sweep CTE itself).
pub(crate) async fn try_claim_and_fire(
    tx: &mut Transaction<'_, Postgres>,
    batch_id: i64,
) -> Result<()> {
    let conn: &mut PgConnection = tx;
    let claimed: Option<(i32, i32, i32, i32, Option<serde_json::Value>, i64)> = sqlx::query_as(
        r#"
        UPDATE eddyq_batches
           SET callback_enqueued_at = NOW(),
               state = 'complete',
               finalized_at = NOW()
         WHERE id = $1
           AND callback_enqueued_at IS NULL
           AND completed + failed + cancelled = total
     RETURNING total, completed, failed, cancelled, on_complete,
               (EXTRACT(EPOCH FROM (NOW() - created_at)) * 1000)::bigint
        "#,
    )
    .bind(batch_id)
    .fetch_optional(conn)
    .await?;

    let Some((total, completed, failed, cancelled, on_complete, duration_ms)) = claimed else {
        return Ok(()); // not done yet, or already claimed
    };
    let Some(spec_value) = on_complete else {
        return Ok(()); // batch had no callback configured
    };
    let spec: DynEnqueue = serde_json::from_value(spec_value)?;
    let summary = BatchSettleSummary {
        total: total as i64,
        completed: completed as i64,
        failed: failed as i64,
        cancelled: cancelled as i64,
        duration_ms,
    };
    let callback = build_callback_dyn(spec, batch_id, summary);
    enqueue_dyn_in_tx(tx, callback).await?;
    Ok(())
}

/// Stamp the user's callback `DynEnqueue` with the batch envelope and a
/// deterministic `unique_key`. The unique_key is the second line of defense
/// against double-fire — the primary guard is the `callback_enqueued_at IS NULL`
/// claim predicate in `settle_terminal`.
pub(crate) fn build_callback_dyn(
    mut spec: DynEnqueue,
    batch_id: i64,
    summary: BatchSettleSummary,
) -> DynEnqueue {
    let envelope = serde_json::json!({
        "batchId": batch_id,
        "total": summary.total,
        "completed": summary.completed,
        "failed": summary.failed,
        "cancelled": summary.cancelled,
        "durationMs": summary.duration_ms,
    });

    if let serde_json::Value::Object(ref mut map) = spec.payload {
        map.insert("_eddyq_batch".to_string(), envelope);
    } else {
        let original = std::mem::replace(&mut spec.payload, serde_json::Value::Null);
        spec.payload = serde_json::json!({
            "_eddyq_batch": envelope,
            "_payload": original,
        });
    }

    spec.unique_key = Some(format!("eddyq.batch.{}.callback", batch_id));
    spec
}
