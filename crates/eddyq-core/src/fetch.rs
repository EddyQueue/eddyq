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
    /// Number of times this row has been rescued from a stalled (worker-lost)
    /// state. Distinct from `attempt`, which counts completed handler runs.
    pub stalled_count: i32,
    pub max_stalled_count: i32,
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
///   2. Find groups with pending work (loose index scan); lock their rows
///      with SKIP LOCKED; compute per-group slots =
///      min(concurrency remaining, floor(refilled tokens)).
///   3. For each group with slots > 0, lock up to `slots` candidates from
///      that group specifically.
///   4. UPDATE the accepted jobs to 'running', bump running_count + token
///      balances on their groups.
///
/// All phases share one transaction. Group rows are taken with `FOR UPDATE
/// SKIP LOCKED`, so two concurrent fetchers *partition* the active groups
/// instead of queueing on each other's commits — a group whose row is held by
/// a peer is simply served on the next poll. Counter updates stay correct
/// because only the lock holder claims from (and bumps counters for) a group.
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
                        Some(std::time::Duration::from_millis(
                            u64::try_from(ms).unwrap_or(0),
                        )),
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
        let take =
            i64::from(q_slots).min(i64::try_from(batch_size - accepted.len()).unwrap_or(i64::MAX));
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
    //
    // The distinct-key walk is a recursive "loose index scan": Postgres has
    // no native skip-scan, so a plain SELECT DISTINCT walks every pending
    // grouped index entry on every fetch — O(pending jobs) instead of
    // O(groups). Each recursive step seeks the next group_key past the
    // current one via eddyq_jobs_group, so a deep backlog in one group costs
    // one index seek, not one scan per row.
    let active_group_keys: Vec<String> = if remaining == 0 {
        vec![]
    } else {
        let rows: Vec<(String,)> = sqlx::query_as(
            r#"
            WITH RECURSIVE walk AS (
                (SELECT j.group_key AS key
                   FROM eddyq_jobs j
                  WHERE j.state = 'pending'
                    AND j.scheduled_at <= NOW()
                    AND j.group_key IS NOT NULL
                    AND j.kind = ANY($1)
                    AND j.queue = ANY($2)
               ORDER BY j.group_key
                  LIMIT 1)
              UNION ALL
                SELECT (SELECT j.group_key
                          FROM eddyq_jobs j
                         WHERE j.state = 'pending'
                           AND j.scheduled_at <= NOW()
                           AND j.group_key > w.key
                           AND j.kind = ANY($1)
                           AND j.queue = ANY($2)
                      ORDER BY j.group_key
                         LIMIT 1)
                  FROM walk w
                 WHERE w.key IS NOT NULL
            )
            SELECT key FROM walk WHERE key IS NOT NULL
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
               FOR UPDATE SKIP LOCKED
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

        // A group key with pending work but no `eddyq_groups` row is uncapped,
        // and until now it was also unclaimable: the row is only materialized
        // when some group rule matches the key (`group::materialize_from_rule`),
        // so a group with no cap and no matching rule never appeared here and
        // its jobs sat pending forever.
        //
        // "No row" has to be told apart from "row held by a peer fetcher",
        // which the locking read above cannot do -- SKIP LOCKED returns neither.
        // Conflating them would be worse than the bug: a capped group whose row
        // is momentarily locked would be treated as unlimited and two fetchers
        // would both claim from it, blowing past its cap. Hence this second,
        // non-locking read; it is a primary-key lookup and only ever finds
        // anything for genuinely new groups, since phase 4 creates the row on
        // first claim.
        let existing: Vec<(String,)> =
            sqlx::query_as("SELECT key FROM eddyq_groups WHERE key = ANY($1)")
                .bind(&group_keys)
                .fetch_all(&mut *tx)
                .await?;
        let existing: std::collections::HashSet<String> =
            existing.into_iter().map(|(k,)| k).collect();

        for key in &group_keys {
            if !existing.contains(key) {
                group_slots.insert(
                    key.clone(),
                    GroupState {
                        slots: i32::MAX,
                        refilled_tokens: 0.0,
                        rate_limited: false,
                    },
                );
            }
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
        let take_n = i64::from(state.slots)
            .min(i64::try_from(batch_size - accepted.len()).unwrap_or(i64::MAX));
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
    type ClaimedRow = (
        JobId,
        String,
        serde_json::Value,
        i32,
        i32,
        i32,
        i32,
        Option<String>,
        String,
    );
    let accepted_ids: Vec<JobId> = accepted.iter().map(|(id, _, _)| *id).collect();
    let claimed: Vec<ClaimedRow> = sqlx::query_as(
        r#"
            UPDATE eddyq_jobs AS j
               SET state        = 'running',
                   attempt      = j.attempt + 1,
                   heartbeat_at = NOW(),
                   worker_id    = $2
             WHERE j.id = ANY($1)
         RETURNING j.id, j.kind, j.payload, j.attempt, j.max_attempts, j.stalled_count, j.max_stalled_count, j.group_key, j.queue
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
    for (_, _, _, _, _, _, _, group_key, qname) in &claimed {
        if let Some(g) = group_key {
            *group_deltas.entry(g.clone()).or_insert(0) += 1;
        }
        *queue_deltas.entry(qname.clone()).or_insert(0) += 1;
    }
    for (key, delta) in &group_deltas {
        // Single statement per group: the token balance is folded into the
        // running_count upsert rather than issued as a second UPDATE. Every
        // rate-limited claim used to rewrite the (hot, shared) group row
        // twice per batch — on a busy throttled lane that doubled the row
        // versions, WAL, and vacuum work for no behavioral difference.
        let rate_limited = group_slots.get(key).is_some_and(|s| s.rate_limited);
        if rate_limited {
            let state = group_slots.get(key).expect("checked above");
            let remaining = (state.refilled_tokens - f64::from(*delta)).max(0.0);
            sqlx::query(
                r#"
                INSERT INTO eddyq_groups (key, running_count, tokens, tokens_refilled_at)
                VALUES ($1, $2, $3, $4)
                ON CONFLICT (key) DO UPDATE
                   SET running_count      = eddyq_groups.running_count + EXCLUDED.running_count,
                       tokens             = EXCLUDED.tokens,
                       tokens_refilled_at = EXCLUDED.tokens_refilled_at,
                       updated_at         = NOW()
                "#,
            )
            .bind(key)
            .bind(delta)
            .bind(remaining)
            .bind(now_utc)
            .execute(&mut *tx)
            .await?;
        } else {
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
            |(
                id,
                kind,
                payload,
                attempt,
                max_attempts,
                stalled_count,
                max_stalled_count,
                group_key,
                queue,
            )| {
                let timeout = queue_timeout.get(&queue).and_then(|o| *o);
                ClaimedJob {
                    id,
                    kind,
                    payload,
                    attempt,
                    max_attempts,
                    stalled_count,
                    max_stalled_count,
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
    // If the job belongs to a batch, settle the batch counter in the same tx.
    let mut tx = pool.begin().await?;
    let row: Option<(Option<i64>,)> = sqlx::query_as(
        r#"
        UPDATE eddyq_jobs
           SET state        = 'cancelled',
               finalized_at = NOW()
         WHERE id = $1
           AND state = 'pending'
     RETURNING batch_id
        "#,
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;

    let cancelled = row.is_some();
    if let Some((Some(batch_id),)) = row {
        crate::batch::settle_terminal(&mut tx, batch_id, crate::batch::TerminalOutcome::Cancelled)
            .await?;
    }
    tx.commit().await?;
    Ok(cancelled)
}

/// Per-state retention policy. `*_secs = None` and `*_count = None` together
/// mean "keep forever." When both are set, OR semantics apply: a row is reaped
/// if it exceeds *either* the age window *or* the count cap. Gives Redis users
/// a hard memory bound that age alone can't.
#[derive(Debug, Clone, Copy, Default)]
pub struct Retention {
    pub completed_secs: Option<u64>,
    pub failed_secs: Option<u64>,
    pub cancelled_secs: Option<u64>,
    /// Delete finalized batch rows (`state='complete'`) past this age. Pending
    /// batches are never reaped. Job rows hold a `batch_id` FK with
    /// `ON DELETE SET NULL`, so deleting a batch row is safe regardless of
    /// whether its jobs have already been reaped.
    pub batch_secs: Option<u64>,
    /// Keep at most this many completed jobs (newest by `finalized_at`).
    pub completed_count: Option<i64>,
    pub failed_count: Option<i64>,
    pub cancelled_count: Option<i64>,
    pub batch_count: Option<i64>,
}

impl Retention {
    /// True when no field would ever cause a delete — cleanup can short-circuit.
    pub fn is_disabled(&self) -> bool {
        self.completed_secs.is_none()
            && self.failed_secs.is_none()
            && self.cancelled_secs.is_none()
            && self.batch_secs.is_none()
            && self.completed_count.is_none()
            && self.failed_count.is_none()
            && self.cancelled_count.is_none()
            && self.batch_count.is_none()
    }
}

/// Delete finalized rows that exceed the configured retention (age OR count).
/// Returns (completed_jobs, failed_jobs, cancelled_jobs, batches).
pub async fn cleanup(pool: &PgPool, retention: Retention) -> Result<(u64, u64, u64, u64)> {
    let mut completed = 0u64;
    let mut failed = 0u64;
    let mut cancelled = 0u64;

    for (state, maybe_secs, maybe_count, out) in [
        (
            "completed",
            retention.completed_secs,
            retention.completed_count,
            &mut completed,
        ),
        (
            "failed",
            retention.failed_secs,
            retention.failed_count,
            &mut failed,
        ),
        (
            "cancelled",
            retention.cancelled_secs,
            retention.cancelled_count,
            &mut cancelled,
        ),
    ] {
        *out = cleanup_jobs_state(pool, state, maybe_secs, maybe_count).await?;
    }

    let batches = cleanup_batches(pool, retention.batch_secs, retention.batch_count).await?;

    Ok((completed, failed, cancelled, batches))
}

async fn cleanup_jobs_state(
    pool: &PgPool,
    state: &str,
    age_secs: Option<u64>,
    count: Option<i64>,
) -> Result<u64> {
    match (age_secs, count) {
        (None, None) => Ok(0),
        // Age-only — fast path, uses eddyq_jobs_finalized partial index.
        (Some(secs), None) => {
            let secs = i64::try_from(secs).unwrap_or(i64::MAX);
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
            Ok(res.rows_affected())
        }
        // Count-only — keep newest N, delete the rest. Window scan over
        // finalized rows in this state; bounded by the partial index.
        (None, Some(count)) => {
            let count = count.max(0);
            let res = sqlx::query(
                r#"
                WITH ranked AS (
                  SELECT id, row_number() OVER (ORDER BY finalized_at DESC) AS rn
                    FROM eddyq_jobs
                   WHERE state = $1
                     AND finalized_at IS NOT NULL
                )
                DELETE FROM eddyq_jobs
                 WHERE id IN (SELECT id FROM ranked WHERE rn > $2)
                "#,
            )
            .bind(state)
            .bind(count)
            .execute(pool)
            .await?;
            Ok(res.rows_affected())
        }
        // Both — OR semantics: delete if expired by age OR beyond the newest N.
        (Some(secs), Some(count)) => {
            let secs = i64::try_from(secs).unwrap_or(i64::MAX);
            let count = count.max(0);
            let res = sqlx::query(
                r#"
                WITH ranked AS (
                  SELECT id, finalized_at,
                         row_number() OVER (ORDER BY finalized_at DESC) AS rn
                    FROM eddyq_jobs
                   WHERE state = $1
                     AND finalized_at IS NOT NULL
                )
                DELETE FROM eddyq_jobs
                 WHERE id IN (
                   SELECT id FROM ranked
                    WHERE finalized_at < NOW() - make_interval(secs => $2)
                       OR rn > $3
                 )
                "#,
            )
            .bind(state)
            .bind(secs)
            .bind(count)
            .execute(pool)
            .await?;
            Ok(res.rows_affected())
        }
    }
}

async fn cleanup_batches(pool: &PgPool, age_secs: Option<u64>, count: Option<i64>) -> Result<u64> {
    match (age_secs, count) {
        (None, None) => Ok(0),
        (Some(secs), None) => {
            let secs = i64::try_from(secs).unwrap_or(i64::MAX);
            let res = sqlx::query(
                r#"
                DELETE FROM eddyq_batches
                 WHERE state = 'complete'
                   AND finalized_at IS NOT NULL
                   AND finalized_at < NOW() - make_interval(secs => $1)
                "#,
            )
            .bind(secs)
            .execute(pool)
            .await?;
            Ok(res.rows_affected())
        }
        (None, Some(count)) => {
            let count = count.max(0);
            let res = sqlx::query(
                r#"
                WITH ranked AS (
                  SELECT id, row_number() OVER (ORDER BY finalized_at DESC) AS rn
                    FROM eddyq_batches
                   WHERE state = 'complete'
                     AND finalized_at IS NOT NULL
                )
                DELETE FROM eddyq_batches
                 WHERE id IN (SELECT id FROM ranked WHERE rn > $1)
                "#,
            )
            .bind(count)
            .execute(pool)
            .await?;
            Ok(res.rows_affected())
        }
        (Some(secs), Some(count)) => {
            let secs = i64::try_from(secs).unwrap_or(i64::MAX);
            let count = count.max(0);
            let res = sqlx::query(
                r#"
                WITH ranked AS (
                  SELECT id, finalized_at,
                         row_number() OVER (ORDER BY finalized_at DESC) AS rn
                    FROM eddyq_batches
                   WHERE state = 'complete'
                     AND finalized_at IS NOT NULL
                )
                DELETE FROM eddyq_batches
                 WHERE id IN (
                   SELECT id FROM ranked
                    WHERE finalized_at < NOW() - make_interval(secs => $1)
                       OR rn > $2
                 )
                "#,
            )
            .bind(secs)
            .bind(count)
            .execute(pool)
            .await?;
            Ok(res.rows_affected())
        }
    }
}

/// Ad-hoc retention sweep. Deletes up to `limit` finalized jobs in the
/// given state older than `grace_secs`. The CTE caps the DELETE to `limit`
/// rows by selecting IDs first — `DELETE ... LIMIT` isn't valid Postgres.
/// Returns the count actually deleted.
pub async fn clean_jobs(pool: &PgPool, state: &str, grace_secs: u64, limit: u32) -> Result<u64> {
    if limit == 0 {
        return Ok(0);
    }
    let grace_secs = i64::try_from(grace_secs).unwrap_or(i64::MAX);
    let limit = i64::from(limit);
    let res = sqlx::query(
        r#"
        WITH victims AS (
            SELECT id FROM eddyq_jobs
             WHERE state = $1
               AND finalized_at IS NOT NULL
               AND finalized_at < NOW() - make_interval(secs => $2)
             ORDER BY finalized_at ASC
             LIMIT $3
        )
        DELETE FROM eddyq_jobs WHERE id IN (SELECT id FROM victims)
        "#,
    )
    .bind(state)
    .bind(grace_secs)
    .bind(limit)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
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
    // counter, and (if the job belongs to a batch) settles the batch counter
    // — all in one transaction.
    let mut tx = pool.begin().await?;
    let row: Option<(Option<String>, String, Option<i64>)> = sqlx::query_as(
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
     RETURNING group_key, queue, batch_id
        "#,
    )
    .bind(id)
    .bind(worker_id)
    .bind(result)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((group_key, queue, batch_id)) = row {
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
        if let Some(b) = batch_id {
            crate::batch::settle_terminal(&mut tx, b, crate::batch::TerminalOutcome::Completed)
                .await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

/// Update heartbeat for multiple in-flight jobs in a single query.
/// Returns the number of rows updated.
pub async fn update_heartbeat_batch(pool: &PgPool, ids: &[i64]) -> Result<u64> {
    if ids.is_empty() {
        return Ok(0);
    }
    let res = sqlx::query(
        r#"
        UPDATE eddyq_jobs
           SET heartbeat_at = NOW()
         WHERE id = ANY($1) AND state = 'running'
        "#,
    )
    .bind(ids)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Sweep running jobs whose heartbeat is older than `stale_after`. Each swept
/// row has its `stalled_count` bumped; rows whose new `stalled_count` exceeds
/// `max_stalled_count` are marked failed, the rest are returned to `pending`
/// with `attempt` decremented (the prior attempt didn't run to a verdict, so
/// it shouldn't burn the handler-throw budget). In both branches the group
/// counter is decremented; rows that went terminal-failed also bump their
/// batch's failed counter and the batch's `on_complete` callback may fire
/// (in this same transaction) if the sweep was the last terminal transition
/// needed. Returns the number of rows touched.
pub async fn sweep_stale(pool: &PgPool, stale_after: std::time::Duration) -> Result<u64> {
    let secs = i64::try_from(stale_after.as_secs()).unwrap_or(i64::MAX);
    let error_entry = serde_json::json!({
        "at": chrono::Utc::now(),
        "message": "worker lost contact — job recovered",
    });

    let mut tx = pool.begin().await?;

    // Sweep + decrement group/queue counters + bump batch failed counters,
    // all in one statement. Returns (recovered, distinct batch_ids that had
    // a terminal-fail bump).
    let (recovered, bumped_batches): (i64, Vec<i64>) = sqlx::query_as(
        r#"
        WITH swept AS (
            UPDATE eddyq_jobs
               SET stalled_count = stalled_count + 1,
                   state         = CASE WHEN stalled_count + 1 > max_stalled_count
                                        THEN 'failed' ELSE 'pending' END,
                   attempt       = CASE WHEN stalled_count + 1 > max_stalled_count
                                        THEN attempt ELSE GREATEST(attempt - 1, 0) END,
                   heartbeat_at  = NULL,
                   worker_id     = NULL,
                   errors        = errors || $2::jsonb,
                   finalized_at  = CASE WHEN stalled_count + 1 > max_stalled_count
                                        THEN NOW() ELSE NULL END
             WHERE state = 'running'
               AND heartbeat_at < NOW() - make_interval(secs => $1)
         RETURNING group_key, queue, batch_id, state
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
        batch_failures AS (
            SELECT batch_id, COUNT(*)::int AS delta
              FROM swept
             WHERE batch_id IS NOT NULL
               AND state = 'failed'
          GROUP BY batch_id
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
        ),
        _bump_batches AS (
            UPDATE eddyq_batches b
               SET failed = b.failed + bf.delta
              FROM batch_failures bf
             WHERE b.id = bf.batch_id
            RETURNING b.id
        )
        SELECT
            (SELECT COUNT(*) FROM swept)::bigint,
            COALESCE(
                (SELECT ARRAY_AGG(id ORDER BY id) FROM _bump_batches),
                ARRAY[]::bigint[]
            )
        "#,
    )
    .bind(secs)
    .bind(error_entry)
    .fetch_one(&mut *tx)
    .await?;

    // For each batch whose counter we just bumped, attempt to claim and fire
    // the callback. Idempotent under concurrent settlers.
    for batch_id in bumped_batches {
        crate::batch::try_claim_and_fire(&mut tx, batch_id).await?;
    }

    tx.commit().await?;
    Ok(u64::try_from(recovered).unwrap_or(0))
}

/// Proactively reclaim a known list of in-flight jobs — used by force-mode
/// shutdown so jobs this pod has claimed but won't get to finish are made
/// re-eligible for other pods *immediately*, instead of waiting one
/// `stale_after` cycle for the heartbeat sweeper.
///
/// Filtered by `state = 'running'`, so this is race-safe: if a worker on
/// this pod already finalized a row to `completed`/`failed`/`cancelled`
/// before we reclaim, that row is left alone. Returns the count actually
/// reclaimed.
///
/// Mirrors `sweep_stale` exactly — same counter decrements, same batch-fail
/// bumps, same callback claim + fire. Only the WHERE clause differs.
pub async fn reclaim_in_flight(pool: &PgPool, ids: &[JobId]) -> Result<u64> {
    if ids.is_empty() {
        return Ok(0);
    }
    let error_entry = serde_json::json!({
        "at": chrono::Utc::now(),
        "message": "worker shutting down — job recovered",
    });

    let mut tx = pool.begin().await?;

    let (recovered, bumped_batches): (i64, Vec<i64>) = sqlx::query_as(
        r#"
        WITH swept AS (
            UPDATE eddyq_jobs
               SET stalled_count = stalled_count + 1,
                   state         = CASE WHEN stalled_count + 1 > max_stalled_count
                                        THEN 'failed' ELSE 'pending' END,
                   attempt       = CASE WHEN stalled_count + 1 > max_stalled_count
                                        THEN attempt ELSE GREATEST(attempt - 1, 0) END,
                   heartbeat_at  = NULL,
                   worker_id     = NULL,
                   errors        = errors || $2::jsonb,
                   finalized_at  = CASE WHEN stalled_count + 1 > max_stalled_count
                                        THEN NOW() ELSE NULL END
             WHERE state = 'running'
               AND id = ANY($1)
         RETURNING group_key, queue, batch_id, state
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
        batch_failures AS (
            SELECT batch_id, COUNT(*)::int AS delta
              FROM swept
             WHERE batch_id IS NOT NULL
               AND state = 'failed'
          GROUP BY batch_id
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
        ),
        _bump_batches AS (
            UPDATE eddyq_batches b
               SET failed = b.failed + bf.delta
              FROM batch_failures bf
             WHERE b.id = bf.batch_id
            RETURNING b.id
        )
        SELECT
            (SELECT COUNT(*) FROM swept)::bigint,
            COALESCE(
                (SELECT ARRAY_AGG(id ORDER BY id) FROM _bump_batches),
                ARRAY[]::bigint[]
            )
        "#,
    )
    .bind(ids)
    .bind(error_entry)
    .fetch_one(&mut *tx)
    .await?;

    for batch_id in bumped_batches {
        crate::batch::try_claim_and_fire(&mut tx, batch_id).await?;
    }

    tx.commit().await?;
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

    // We only want to settle the batch counter on the *terminal* branch (no
    // more retries). The retry branch returns the job to 'pending'; we record
    // batch_id only in the else branch via `terminal_batch_id`.
    let mut terminal_batch_id: Option<i64> = None;

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
        let r: Option<(Option<String>, String, Option<i64>)> = sqlx::query_as(
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
         RETURNING group_key, queue, batch_id
            "#,
        )
        .bind(id)
        .bind(error_entry)
        .bind(worker_id)
        .fetch_optional(&mut *tx)
        .await?;
        r.map(|(g, q, b)| {
            terminal_batch_id = b;
            (g, q)
        })
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
        if let Some(b) = terminal_batch_id {
            crate::batch::settle_terminal(&mut tx, b, crate::batch::TerminalOutcome::Failed)
                .await?;
        }
    }
    tx.commit().await?;
    Ok(())
}
