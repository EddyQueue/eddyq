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
    pub worker_id: Uuid,
}

/// Claim up to `batch_size` pending jobs, respecting per-group concurrency caps.
///
/// Two-phase:
///   1. Lock up to `batch_size * 4` candidate job rows with FOR UPDATE SKIP LOCKED.
///   2. Lock the relevant `eddyq_groups` rows (plain FOR UPDATE — serializes
///      same-group claims between concurrent fetchers). Accept candidates in
///      priority order until each group's `max_concurrency - running_count`
///      budget is exhausted.
///   3. UPDATE the accepted jobs to 'running' and upsert group counters.
///
/// All three steps share one transaction, so another concurrent fetcher that
/// tries to touch the same groups will block on the eddyq_groups FOR UPDATE
/// until we commit — then see our updated running_count.
pub async fn claim_batch(
    pool: &PgPool,
    worker_id: Uuid,
    batch_size: usize,
) -> Result<Vec<ClaimedJob>> {
    if batch_size == 0 {
        return Ok(vec![]);
    }
    let over_fetch = i64::try_from(batch_size.saturating_mul(4)).unwrap_or(i64::MAX);

    let mut tx = pool.begin().await?;

    // Step 1: candidate job rows (locked, but not yet updated).
    let candidates: Vec<(JobId, Option<String>)> = sqlx::query_as(
        r#"
        SELECT j.id, j.group_key
          FROM eddyq_jobs j
         WHERE j.state = 'pending'
           AND j.scheduled_at <= NOW()
      ORDER BY j.priority DESC, j.scheduled_at ASC, j.id ASC
         LIMIT $1
         FOR UPDATE OF j SKIP LOCKED
        "#,
    )
    .bind(over_fetch)
    .fetch_all(&mut *tx)
    .await?;

    if candidates.is_empty() {
        tx.rollback().await?;
        return Ok(vec![]);
    }

    // Step 2: lock + read caps for every group that shows up.
    let group_keys: Vec<String> = {
        let mut keys: Vec<String> =
            candidates.iter().filter_map(|(_, g)| g.clone()).collect();
        keys.sort();
        keys.dedup();
        keys
    };

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

    // Step 3: decide which candidates to claim — priority order preserved,
    // each group's slot budget depleted as we go.
    let mut accepted: Vec<(JobId, Option<String>)> = Vec::with_capacity(batch_size);
    for (id, group) in candidates {
        if accepted.len() >= batch_size {
            break;
        }
        match &group {
            None => accepted.push((id, None)),
            Some(g) => {
                // Groups with no configured row are implicitly unlimited; treat
                // as if they have a large slot budget so we still track
                // running_count when we bump.
                let state = group_slots.entry(g.clone()).or_insert(GroupState {
                    slots: i32::MAX,
                    refilled_tokens: 0.0,
                    rate_limited: false,
                });
                if state.slots > 0 {
                    state.slots -= 1;
                    accepted.push((id, Some(g.clone())));
                }
            }
        }
    }

    if accepted.is_empty() {
        tx.rollback().await?;
        return Ok(vec![]);
    }

    // Step 4: UPDATE accepted jobs to 'running'; upsert group counters.
    let accepted_ids: Vec<JobId> = accepted.iter().map(|(id, _)| *id).collect();
    let claimed: Vec<(JobId, String, serde_json::Value, i32, i32, Option<String>)> =
        sqlx::query_as(
            r#"
            UPDATE eddyq_jobs AS j
               SET state        = 'running',
                   attempt      = j.attempt + 1,
                   heartbeat_at = NOW(),
                   worker_id    = $2
             WHERE j.id = ANY($1)
         RETURNING j.id, j.kind, j.payload, j.attempt, j.max_attempts, j.group_key
            "#,
        )
        .bind(&accepted_ids)
        .bind(worker_id)
        .fetch_all(&mut *tx)
        .await?;

    // Aggregate per-group deltas and upsert running_count. For rate-limited
    // groups, also write back the decremented token balance and refill timestamp.
    let mut deltas: HashMap<String, i32> = HashMap::new();
    for (_, _, _, _, _, group_key) in &claimed {
        if let Some(g) = group_key {
            *deltas.entry(g.clone()).or_insert(0) += 1;
        }
    }
    for (key, delta) in &deltas {
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

        // Token writeback for rate-limited groups.
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

    tx.commit().await?;

    Ok(claimed
        .into_iter()
        .map(|(id, kind, payload, attempt, max_attempts, group_key)| ClaimedJob {
            id,
            kind,
            payload,
            attempt,
            max_attempts,
            group_key,
            worker_id,
        })
        .collect())
}

// Silence unused-import warning when the file below doesn't reference this alias.
#[allow(dead_code)]
fn _assert_tx_type(_: &mut Transaction<'_, Postgres>) {}

pub async fn mark_completed(pool: &PgPool, id: JobId, worker_id: Uuid) -> Result<()> {
    // Gate on (state='running' AND worker_id = our uuid) so a worker whose
    // heartbeat was swept can't clobber the job state after another worker
    // picked it up. The GREATEST(x, 0) clamp on the counter is defensive:
    // the sweeper already decremented when it reset the job.
    sqlx::query(
        r#"
        WITH job AS (
            UPDATE eddyq_jobs
               SET state        = 'completed',
                   heartbeat_at = NULL,
                   worker_id    = NULL,
                   completed_at = NOW()
             WHERE id = $1
               AND state = 'running'
               AND worker_id = $2
         RETURNING group_key
        )
        UPDATE eddyq_groups g
           SET running_count = GREATEST(g.running_count - 1, 0),
               updated_at    = NOW()
          FROM job
         WHERE g.key = job.group_key
        "#,
    )
    .bind(id)
    .bind(worker_id)
    .execute(pool)
    .await?;
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

    // Two-step to keep the decrements tidy: update the jobs, collect their
    // group_keys with a CTE, then update eddyq_groups in aggregate.
    let (recovered,): (i64,) = sqlx::query_as(
        r#"
        WITH swept AS (
            UPDATE eddyq_jobs
               SET state        = CASE WHEN attempt >= max_attempts THEN 'failed' ELSE 'pending' END,
                   heartbeat_at = NULL,
                   worker_id    = NULL,
                   errors       = errors || $2::jsonb,
                   completed_at = CASE WHEN attempt >= max_attempts THEN NOW() ELSE NULL END
             WHERE state = 'running'
               AND heartbeat_at < NOW() - make_interval(secs => $1)
         RETURNING group_key
        ),
        decrements AS (
            SELECT group_key AS key, COUNT(*)::int AS delta
              FROM swept
             WHERE group_key IS NOT NULL
          GROUP BY group_key
        ),
        _drop AS (
            UPDATE eddyq_groups g
               SET running_count = GREATEST(g.running_count - d.delta, 0),
                   updated_at    = NOW()
              FROM decrements d
             WHERE g.key = d.key
            RETURNING g.key
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
    error: &str,
    retry_at: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<()> {
    let error_entry = serde_json::json!({
        "at": chrono::Utc::now(),
        "message": error,
    });

    if let Some(at) = retry_at {
        sqlx::query(
            r#"
            WITH job AS (
                UPDATE eddyq_jobs
                   SET state        = 'pending',
                       heartbeat_at = NULL,
                       worker_id    = NULL,
                       scheduled_at = $2,
                       errors       = errors || $3::jsonb
                 WHERE id = $1
                   AND state = 'running'
                   AND worker_id = $4
             RETURNING group_key
            )
            UPDATE eddyq_groups g
               SET running_count = GREATEST(g.running_count - 1, 0),
                   updated_at    = NOW()
              FROM job
             WHERE g.key = job.group_key
            "#,
        )
        .bind(id)
        .bind(at)
        .bind(error_entry)
        .bind(worker_id)
        .execute(pool)
        .await?;
    } else {
        sqlx::query(
            r#"
            WITH job AS (
                UPDATE eddyq_jobs
                   SET state        = 'failed',
                       heartbeat_at = NULL,
                       worker_id    = NULL,
                       errors       = errors || $2::jsonb,
                       completed_at = NOW()
                 WHERE id = $1
                   AND state = 'running'
                   AND worker_id = $3
             RETURNING group_key
            )
            UPDATE eddyq_groups g
               SET running_count = GREATEST(g.running_count - 1, 0),
                   updated_at    = NOW()
              FROM job
             WHERE g.key = job.group_key
            "#,
        )
        .bind(id)
        .bind(error_entry)
        .bind(worker_id)
        .execute(pool)
        .await?;
    }
    Ok(())
}
