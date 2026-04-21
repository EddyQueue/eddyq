use std::collections::HashMap;

use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::{error::Result, job::JobId};

#[derive(Debug, Clone)]
pub struct ClaimedJob {
    pub id: JobId,
    pub kind: String,
    pub payload: serde_json::Value,
    pub attempt: i32,
    pub max_attempts: i32,
    pub group_key: Option<String>,
    pub queue: String,
    pub worker_id: Uuid,
    /// Resolved per-job timeout: the queue's `default_timeout_ms` at claim
    /// time, or `None` if no timeout is configured for this queue.
    pub timeout: Option<std::time::Duration>,
}

/// Claim up to `batch_size` pending jobs, respecting per-group concurrency
/// caps and per-group token-bucket rate limits.
///
/// Split into independent lanes so capped "slowlane" groups can't starve
/// "fastlane" (ungrouped) work when the slowlane has a large backlog at equal
/// priority:
///
///   1. Lock ungrouped candidates up to the full `batch_size` (fastlane).
///   2. Find groups with pending work; lock their rows (serializes same-group
///      claims between concurrent fetchers); compute per-group slots =
///      min(concurrency remaining, floor(refilled tokens)).
///   3. For each group with slots > 0, lock up to `slots` candidates from
///      that group specifically.
///   4. UPDATE the accepted jobs to 'running', bump running_count + token
///      balances on their groups.
///
/// All phases share one transaction: another concurrent fetcher that tries to
/// touch the same groups blocks on the `eddyq_groups` FOR UPDATE until we
/// commit, then sees the updated counters.
pub async fn claim_batch(
    pool: &PgPool,
    worker_id: Uuid,
    batch_size: usize,
    kinds: &[String],
    queues: &[String],
) -> Result<Vec<ClaimedJob>> {
    if batch_size == 0 || kinds.is_empty() || queues.is_empty() {
        return Ok(vec![]);
    }
    let kinds_vec: Vec<String> = kinds.to_vec();

    let mut tx = pool.begin().await?;

    // Phase 0 — lock + read cross-process queue caps + default timeouts. For
    // any subscribed queue with a row in eddyq_queues, compute available
    // slots and pick up its default_timeout_ms. Queues without a row are
    // unlimited (i32::MAX) with no timeout.
    let mut queue_budget: HashMap<String, i32> = HashMap::new();
    let mut queue_timeout: HashMap<String, Option<std::time::Duration>> = HashMap::new();
    {
        let rows: Vec<(String, i32, i32, bool, Option<i32>)> = sqlx::query_as(
            r#"
            SELECT name, running_count, max_concurrency, paused, default_timeout_ms
              FROM eddyq_queues
             WHERE name = ANY($1)
          ORDER BY name
               FOR UPDATE
            "#,
        )
        .bind(queues)
        .fetch_all(&mut *tx)
        .await?;
        for (name, running, max, paused, timeout_ms) in rows {
            let slots = if paused { 0 } else { (max - running).max(0) };
            queue_budget.insert(name.clone(), slots);
            if let Some(ms) = timeout_ms {
                if ms > 0 {
                    queue_timeout.insert(
                        name,
                        Some(std::time::Duration::from_millis(u64::try_from(ms).unwrap_or(0))),
                    );
                }
            }
        }
    }
    let budget_for = |qname: &str, tbl: &HashMap<String, i32>| -> i32 {
        tbl.get(qname).copied().unwrap_or(i32::MAX)
    };

    // Phase 1 — ungrouped (fastlane). Claim per queue so the per-queue cap
    // is enforced alongside the batch_size cap.
    let mut accepted: Vec<(JobId, Option<String>, String)> = Vec::with_capacity(batch_size);
    'ungrouped: for qname in queues {
        if accepted.len() >= batch_size {
            break;
        }
        let q_slots = budget_for(qname, &queue_budget);
        if q_slots <= 0 {
            continue;
        }
        let take = i64::from(q_slots)
            .min(i64::try_from(batch_size - accepted.len()).unwrap_or(i64::MAX));
        if take <= 0 {
            continue;
        }
        let rows: Vec<(JobId,)> = sqlx::query_as(
            r#"
            SELECT j.id
              FROM eddyq_jobs j
             WHERE j.state = 'pending'
               AND j.scheduled_at <= NOW()
               AND j.group_key IS NULL
               AND j.kind = ANY($2)
               AND j.queue = $3
          ORDER BY j.priority DESC, j.scheduled_at ASC, j.id ASC
             LIMIT $1
             FOR UPDATE OF j SKIP LOCKED
            "#,
        )
        .bind(take)
        .bind(&kinds_vec)
        .bind(qname)
        .fetch_all(&mut *tx)
        .await?;
        let got = i32::try_from(rows.len()).unwrap_or(i32::MAX);
        for (id,) in rows {
            accepted.push((id, None, qname.clone()));
        }
        queue_budget
            .entry(qname.clone())
            .and_modify(|b| *b = (*b - got).max(0));
        if accepted.len() >= batch_size {
            break 'ungrouped;
        }
    }

    let remaining = batch_size.saturating_sub(accepted.len());

    // Phase 2 — find which groups have pending work right now, lock their
    // rows, read their caps/tokens.
    let active_group_keys: Vec<String> = if remaining == 0 {
        vec![]
    } else {
        let rows: Vec<(String,)> = sqlx::query_as(
            r#"
            SELECT DISTINCT j.group_key
              FROM eddyq_jobs j
             WHERE j.state = 'pending'
               AND j.scheduled_at <= NOW()
               AND j.group_key IS NOT NULL
               AND j.kind = ANY($1)
               AND j.queue = ANY($2)
            "#,
        )
        .bind(&kinds_vec)
        .bind(queues)
        .fetch_all(&mut *tx)
        .await?;
        rows.into_iter().map(|(k,)| k).collect()
    };

    let group_keys = active_group_keys;

    // For each group row we lock, track:
    //   - the slot budget (min of concurrency-slots and floor(refilled tokens))
    //   - refilled token value + whether the group is rate-limited
    // The last two are needed post-claim to write back the decremented tokens.
    #[derive(Debug)]
    struct GroupState {
        slots: i32,
        refilled_tokens: f64,
        rate_limited: bool,
    }

    let now_utc = chrono::Utc::now();
    let mut group_slots: HashMap<String, GroupState> = HashMap::new();

    // (key, running_count, max_concurrency, paused, rate_count, rate_period_ms, tokens, tokens_refilled_at)
    type GroupRow = (
        String,
        i32,
        i32,
        bool,
        Option<i32>,
        Option<i32>,
        f64,
        Option<chrono::DateTime<chrono::Utc>>,
    );

    if !group_keys.is_empty() {
        let rows: Vec<GroupRow> = sqlx::query_as(
            r#"
            SELECT key, running_count, max_concurrency, paused,
                   rate_count, rate_period_ms, tokens, tokens_refilled_at
              FROM eddyq_groups
             WHERE key = ANY($1)
         ORDER BY key
               FOR UPDATE
            "#,
        )
        .bind(&group_keys)
        .fetch_all(&mut *tx)
        .await?;

        for (key, running, max, paused, rate_count, rate_period_ms, tokens, refilled_at) in rows {
            let conc_slots = if paused { 0 } else { (max - running).max(0) };

            let (rate_slots, refilled_tokens, rate_limited) = match (rate_count, rate_period_ms) {
                (Some(rc), Some(rp)) if rc > 0 && rp > 0 => {
                    let elapsed_ms = refilled_at
                        .map(|ts| (now_utc - ts).num_milliseconds().max(0) as f64)
                        .unwrap_or(0.0);
                    let refill = elapsed_ms * f64::from(rc) / f64::from(rp);
                    let new_tokens = (tokens + refill).min(f64::from(rc)).max(0.0);
                    let slots = new_tokens.floor() as i64;
                    let slots = i32::try_from(slots.max(0)).unwrap_or(i32::MAX);
                    (slots, new_tokens, true)
                }
                _ => (i32::MAX, 0.0, false),
            };

            let slots = conc_slots.min(rate_slots);
            group_slots.insert(
                key,
                GroupState {
                    slots,
                    refilled_tokens,
                    rate_limited,
                },
            );
        }
    }

    // Phase 3 — for each group with slots > 0, fetch up to `min(group_slots,
    // per-queue_budget)` candidates from that specific group. Stops early if
    // we hit batch_size total.
    for (key, state) in group_slots.iter_mut() {
        if accepted.len() >= batch_size {
            break;
        }
        if state.slots <= 0 {
            continue;
        }
        let take_n = i64::from(state.slots).min(
            i64::try_from(batch_size - accepted.len()).unwrap_or(i64::MAX),
        );
        if take_n <= 0 {
            continue;
        }
        // Fetch candidates for this group, tagged with queue.
        let rows: Vec<(JobId, String)> = sqlx::query_as(
            r#"
            SELECT j.id, j.queue
              FROM eddyq_jobs j
             WHERE j.state = 'pending'
               AND j.scheduled_at <= NOW()
               AND j.group_key = $1
               AND j.kind = ANY($3)
               AND j.queue = ANY($4)
          ORDER BY j.priority DESC, j.scheduled_at ASC, j.id ASC
             LIMIT $2
             FOR UPDATE OF j SKIP LOCKED
            "#,
        )
        .bind(key)
        .bind(take_n)
        .bind(&kinds_vec)
        .bind(queues)
        .fetch_all(&mut *tx)
        .await?;
        // Filter by remaining per-queue budget as we accept.
        for (id, qname) in rows {
            if accepted.len() >= batch_size || state.slots <= 0 {
                break;
            }
            let q_slots = budget_for(&qname, &queue_budget);
            if q_slots <= 0 {
                continue;
            }
            accepted.push((id, Some(key.clone()), qname.clone()));
            state.slots -= 1;
            queue_budget
                .entry(qname)
                .and_modify(|b| *b = (*b - 1).max(0));
        }
    }

    if accepted.is_empty() {
        tx.rollback().await?;
        return Ok(vec![]);
    }

    // Step 4: UPDATE accepted jobs to 'running'; upsert group + queue counters.
    type ClaimedRow = (JobId, String, serde_json::Value, i32, i32, Option<String>, String);
    let accepted_ids: Vec<JobId> = accepted.iter().map(|(id, _, _)| *id).collect();
    let claimed: Vec<ClaimedRow> =
        sqlx::query_as(
            r#"
            UPDATE eddyq_jobs AS j
               SET state        = 'running',
                   attempt      = j.attempt + 1,
                   heartbeat_at = NOW(),
                   worker_id    = $2
             WHERE j.id = ANY($1)
         RETURNING j.id, j.kind, j.payload, j.attempt, j.max_attempts, j.group_key, j.queue
            "#,
        )
        .bind(&accepted_ids)
        .bind(worker_id)
        .fetch_all(&mut *tx)
        .await?;

    // Aggregate per-group deltas and upsert running_count. For rate-limited
    // groups, also write back the decremented token balance and refill timestamp.
    let mut group_deltas: HashMap<String, i32> = HashMap::new();
    let mut queue_deltas: HashMap<String, i32> = HashMap::new();
    for (_, _, _, _, _, group_key, qname) in &claimed {
        if let Some(g) = group_key {
            *group_deltas.entry(g.clone()).or_insert(0) += 1;
        }
        *queue_deltas.entry(qname.clone()).or_insert(0) += 1;
    }
    for (key, delta) in &group_deltas {
        sqlx::query(
            r#"
            INSERT INTO eddyq_groups (key, running_count)
            VALUES ($1, $2)
            ON CONFLICT (key) DO UPDATE
               SET running_count = eddyq_groups.running_count + EXCLUDED.running_count,
                   updated_at    = NOW()
            "#,
        )
        .bind(key)
        .bind(delta)
        .execute(&mut *tx)
        .await?;

        if let Some(state) = group_slots.get(key) {
            if state.rate_limited {
                let remaining = (state.refilled_tokens - f64::from(*delta)).max(0.0);
                sqlx::query(
                    r#"
                    UPDATE eddyq_groups
                       SET tokens             = $2,
                           tokens_refilled_at = $3
                     WHERE key = $1
                    "#,
                )
                .bind(key)
                .bind(remaining)
                .bind(now_utc)
                .execute(&mut *tx)
                .await?;
            }
        }
    }
    // Queue counter upserts — only bump rows that already exist; queues with
    // no row are implicitly unlimited and don't need tracking.
    for (qname, delta) in &queue_deltas {
        sqlx::query(
            r#"
            UPDATE eddyq_queues
               SET running_count = running_count + $2,
                   updated_at    = NOW()
             WHERE name = $1
            "#,
        )
        .bind(qname)
        .bind(delta)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;

    Ok(claimed
        .into_iter()
        .map(
            |(id, kind, payload, attempt, max_attempts, group_key, queue)| {
                let timeout = queue_timeout.get(&queue).and_then(|o| *o);
                ClaimedJob {
                    id,
                    kind,
                    payload,
                    attempt,
                    max_attempts,
                    group_key,
                    queue,
                    worker_id,
                    timeout,
                }
            },
        )
        .collect())
}

// Silence unused-import warning when the file below doesn't reference this alias.
#[allow(dead_code)]
fn _assert_tx_type(_: &mut Transaction<'_, Postgres>) {}

/// Cancel a pending (or future-scheduled) job. No-op if the job is already
/// running or finalized — you can't abort a job mid-execution from eddyq
/// itself; the handler must cooperate for that.
///
/// Returns `true` if the job was cancelled, `false` if it wasn't eligible.
pub async fn cancel(pool: &PgPool, id: JobId) -> Result<bool> {
    // If the job had a group_key, decrement the counter — but only if it was
    // in a state where it had been counted (it wasn't: pending jobs don't
    // contribute to running_count). So we just transition state + finalized_at.
    // NB: `finalized_at` is our unified name for "entered a terminal state" —
    // it's set on completed, failed (no-more-retries), and cancelled jobs.
    let res = sqlx::query(
        r#"
        UPDATE eddyq_jobs
           SET state        = 'cancelled',
               finalized_at = NOW()
         WHERE id = $1
           AND state = 'pending'
        "#,
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Per-state retention policy (seconds). `None` = keep forever.
#[derive(Debug, Clone, Copy, Default)]
pub struct Retention {
    pub completed_secs: Option<u64>,
    pub failed_secs: Option<u64>,
    pub cancelled_secs: Option<u64>,
}

/// Delete finalized jobs older than the per-state retention. Returns
/// (completed_deleted, failed_deleted, cancelled_deleted).
pub async fn cleanup(pool: &PgPool, retention: Retention) -> Result<(u64, u64, u64)> {
    let mut completed = 0u64;
    let mut failed = 0u64;
    let mut cancelled = 0u64;

    for (state, maybe_secs, out) in [
        ("completed", retention.completed_secs, &mut completed),
        ("failed", retention.failed_secs, &mut failed),
        ("cancelled", retention.cancelled_secs, &mut cancelled),
    ] {
        let Some(secs) = maybe_secs else { continue };
        let secs = i64::try_from(secs).unwrap_or(i64::MAX);
        // Uses eddyq_jobs_finalized (finalized_at DESC) partial index.
        let res = sqlx::query(
            r#"
            DELETE FROM eddyq_jobs
             WHERE state = $1
               AND finalized_at IS NOT NULL
               AND finalized_at < NOW() - make_interval(secs => $2)
            "#,
        )
        .bind(state)
        .bind(secs)
        .execute(pool)
        .await?;
        *out = res.rows_affected();
    }

    Ok((completed, failed, cancelled))
}

pub async fn mark_completed(
    pool: &PgPool,
    id: JobId,
    worker_id: Uuid,
    result: Option<serde_json::Value>,
) -> Result<()> {
    // Gate on (state='running' AND worker_id = our uuid) so a worker whose
    // heartbeat was swept can't clobber the job state after another worker
    // picked it up. Decrements both the group counter (if any) AND the queue
    // counter.
    let mut tx = pool.begin().await?;
    let row: Option<(Option<String>, String)> = sqlx::query_as(
        r#"
        UPDATE eddyq_jobs
           SET state        = 'completed',
               heartbeat_at = NULL,
               worker_id    = NULL,
               finalized_at = NOW(),
               result       = $3
         WHERE id = $1
           AND state = 'running'
           AND worker_id = $2
     RETURNING group_key, queue
        "#,
    )
    .bind(id)
    .bind(worker_id)
    .bind(result)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((group_key, queue)) = row {
        if let Some(g) = group_key {
            sqlx::query(
                "UPDATE eddyq_groups SET running_count = GREATEST(running_count - 1, 0), updated_at = NOW() WHERE key = $1",
            )
            .bind(&g)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query(
            "UPDATE eddyq_queues SET running_count = GREATEST(running_count - 1, 0), updated_at = NOW() WHERE name = $1",
        )
        .bind(&queue)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn update_heartbeat(pool: &PgPool, id: JobId, worker_id: Uuid) -> Result<()> {
    sqlx::query(
        r#"
        UPDATE eddyq_jobs
           SET heartbeat_at = NOW()
         WHERE id = $1 AND state = 'running' AND worker_id = $2
        "#,
    )
    .bind(id)
    .bind(worker_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Sweep running jobs whose heartbeat is older than `stale_after`. Jobs that have
/// hit `max_attempts` are marked failed; the rest are returned to `pending` for
/// another worker to pick up. In both cases the group counter is decremented.
/// Returns the number of rows touched.
pub async fn sweep_stale(
    pool: &PgPool,
    stale_after: std::time::Duration,
) -> Result<u64> {
    let secs = i64::try_from(stale_after.as_secs()).unwrap_or(i64::MAX);
    let error_entry = serde_json::json!({
        "at": chrono::Utc::now(),
        "message": "heartbeat timeout — worker presumed dead",
    });

    // Sweep + decrement both group and queue counters in one statement.
    let (recovered,): (i64,) = sqlx::query_as(
        r#"
        WITH swept AS (
            UPDATE eddyq_jobs
               SET state        = CASE WHEN attempt >= max_attempts THEN 'failed' ELSE 'pending' END,
                   heartbeat_at = NULL,
                   worker_id    = NULL,
                   errors       = errors || $2::jsonb,
                   finalized_at = CASE WHEN attempt >= max_attempts THEN NOW() ELSE NULL END
             WHERE state = 'running'
               AND heartbeat_at < NOW() - make_interval(secs => $1)
         RETURNING group_key, queue
        ),
        group_decrements AS (
            SELECT group_key AS key, COUNT(*)::int AS delta
              FROM swept
             WHERE group_key IS NOT NULL
          GROUP BY group_key
        ),
        queue_decrements AS (
            SELECT queue AS name, COUNT(*)::int AS delta
              FROM swept
          GROUP BY queue
        ),
        _drop_groups AS (
            UPDATE eddyq_groups g
               SET running_count = GREATEST(g.running_count - d.delta, 0),
                   updated_at    = NOW()
              FROM group_decrements d
             WHERE g.key = d.key
            RETURNING g.key
        ),
        _drop_queues AS (
            UPDATE eddyq_queues q
               SET running_count = GREATEST(q.running_count - d.delta, 0),
                   updated_at    = NOW()
              FROM queue_decrements d
             WHERE q.name = d.name
            RETURNING q.name
        )
        SELECT COUNT(*) FROM swept
        "#,
    )
    .bind(secs)
    .bind(error_entry)
    .fetch_one(pool)
    .await?;

    Ok(u64::try_from(recovered).unwrap_or(0))
}

/// Mark a job as failed permanently (no more retries) or schedule it for a retry
/// at `retry_at`. When `retry_at` is `Some`, the job goes back to `pending` with
/// `scheduled_at = retry_at`, so the fetcher skips it until that time. In both
/// cases, the group counter is decremented (the slot is returned).
pub async fn mark_failed(
    pool: &PgPool,
    id: JobId,
    worker_id: Uuid,
    error_entry: serde_json::Value,
    retry_at: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<()> {

    let mut tx = pool.begin().await?;

    let row: Option<(Option<String>, String)> = if let Some(at) = retry_at {
        sqlx::query_as(
            r#"
            UPDATE eddyq_jobs
               SET state        = 'pending',
                   heartbeat_at = NULL,
                   worker_id    = NULL,
                   scheduled_at = $2,
                   errors       = errors || $3::jsonb
             WHERE id = $1
               AND state = 'running'
               AND worker_id = $4
         RETURNING group_key, queue
            "#,
        )
        .bind(id)
        .bind(at)
        .bind(error_entry)
        .bind(worker_id)
        .fetch_optional(&mut *tx)
        .await?
    } else {
        sqlx::query_as(
            r#"
            UPDATE eddyq_jobs
               SET state        = 'failed',
                   heartbeat_at = NULL,
                   worker_id    = NULL,
                   errors       = errors || $2::jsonb,
                   finalized_at = NOW()
             WHERE id = $1
               AND state = 'running'
               AND worker_id = $3
         RETURNING group_key, queue
            "#,
        )
        .bind(id)
        .bind(error_entry)
        .bind(worker_id)
        .fetch_optional(&mut *tx)
        .await?
    };

    if let Some((group_key, queue)) = row {
        if let Some(g) = group_key {
            sqlx::query(
                "UPDATE eddyq_groups SET running_count = GREATEST(running_count - 1, 0), updated_at = NOW() WHERE key = $1",
            )
            .bind(&g)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query(
            "UPDATE eddyq_queues SET running_count = GREATEST(running_count - 1, 0), updated_at = NOW() WHERE name = $1",
        )
        .bind(&queue)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}
