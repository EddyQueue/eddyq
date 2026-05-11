//! eddyq-napi — Node.js bindings for `eddyq-client`.
//!
//! Built into a cdylib consumed by the `@eddyq/queue` npm package via NAPI-RS.

#![allow(clippy::missing_safety_doc)]

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use chrono::{DateTime, TimeZone, Utc};
use eddyq_client::{
    Client, ClientConfig, CoreQueue, CoreQueueBuilder, Directive, DynEnqueue, HandlerFailure,
    JobContext, JobResult, JobState, ScheduleDeclaration as CoreScheduleDeclaration, ShutdownMode,
};
use eddyq_core::backend::Backend;
use eddyq_redis::{RedisBackend, RedisConfig};
use napi::{
    bindgen_prelude::*,
    threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode},
};

#[macro_use]
extern crate napi_derive;

/// Returns the eddyq-napi crate version.
#[napi]
#[must_use]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

fn err(e: eddyq_client::Error) -> napi::Error {
    napi::Error::from_reason(e.to_string())
}

/// Run a sqlx-touching future via `spawn_blocking` + `Handle::block_on`.
///
/// Why: sqlx's `Executor` impl for `&mut PgConnection` is not HRTB, so rustc
/// cannot prove an async fn that touches sqlx is `Send`-for-all-lifetimes —
/// the bound the `#[napi]` macro's generated wrapper demands. `spawn_blocking`
/// requires only the outer *closure* to be `Send` (not the future it builds),
/// and `Handle::block_on` has no `Send` bound on the future it polls.
///
/// This costs one blocking-pool slot per call but keeps the rest of the
/// binding simple.
async fn run<F, Fut, T>(f: F) -> Result<T>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<T>>,
    T: Send + 'static,
{
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || handle.block_on(f()))
        .await
        .map_err(|e| napi::Error::from_reason(format!("task join error: {e}")))?
}

// -- Options objects ---------------------------------------------------------

/// Connection options for `Queue.connect`.
#[napi(object)]
pub struct ConnectOptions {
    /// Max sqlx pool connections per process. Default 5.
    ///
    /// **Size this for your fleet, not just one process.**
    /// Each worker process opens `max_connections + 1` connections to Postgres
    /// (the +1 is a dedicated LISTEN socket). At 10 pods with the default you
    /// use 60 connections. Postgres ships with `max_connections = 100`, so
    /// you'll run out fast if you scale without adjusting this.
    ///
    /// Job handlers themselves do NOT hold a connection while running — a
    /// connection is only acquired briefly for fetch, heartbeat ticks, and
    /// complete/fail. So `max_connections` can be much smaller than
    /// `workerConcurrency` (roughly concurrency ÷ 5 is a reasonable starting
    /// point). When using PgBouncer, set this to your per-process PgBouncer
    /// pool allocation, not your raw Postgres `max_connections`.
    pub max_connections: Option<u32>,
    /// Min idle pool connections. Default 0.
    pub min_connections: Option<u32>,
    /// Acquire timeout in milliseconds. Default 30_000.
    pub acquire_timeout_ms: Option<u32>,
    /// Migration line name. Default "main".
    pub line: Option<String>,
    /// Disable the LISTEN/NOTIFY subscriber and use polling only.
    ///
    /// Required when connecting through PgBouncer in **transaction-pooling
    /// mode** — LISTEN needs a persistent session connection, which
    /// transaction pooling does not provide. In poll-only mode the worker
    /// falls back to a configurable poll interval (default 1 s) instead of
    /// being woken immediately on new jobs. Session-mode PgBouncer and direct
    /// Postgres connections do not need this.
    pub poll_only: Option<bool>,
}

/// Options for `Eddyq.shutdown`. All fields optional.
#[napi(object)]
pub struct ShutdownOptions {
    /// Shutdown mode — `"drain"` (default), `"force"`, or `"abandon"`. See
    /// `Eddyq.shutdown` docs for the tradeoffs.
    pub mode: Option<String>,
    /// For `mode="drain"`, max time to wait for in-flight handlers (ms).
    /// Ignored by `force` / `abandon` (which don't await handlers). Default
    /// 30_000.
    pub graceful_timeout_ms: Option<u32>,
}

/// Per-enqueue overrides. All fields optional.
#[napi(object)]
pub struct EnqueueOptions {
    /// Max total attempts before the job is marked failed. Default 3.
    pub max_attempts: Option<i32>,
    /// Priority (higher runs first). Default 0.
    pub priority: Option<i16>,
    /// Named queue to land this job on. Default "default".
    pub queue: Option<String>,
    /// Run no earlier than this time (epoch milliseconds). Default now.
    /// Mutually exclusive with `delayMs` — set one or the other.
    pub scheduled_at_ms: Option<i64>,
    /// Convenience: delay this job by N milliseconds relative to now. Computed
    /// server-side at enqueue time. Mutually exclusive with `scheduledAtMs`.
    pub delay_ms: Option<i64>,
    /// Unique key — duplicate enqueues with the same key are silently skipped.
    pub unique_key: Option<String>,
    /// Group key for per-group concurrency / rate limiting.
    pub group_key: Option<String>,
    /// Admin-visible tags.
    pub tags: Option<Vec<String>>,
    /// Arbitrary JSON metadata attached to the job (not passed to the handler).
    pub metadata: Option<serde_json::Value>,
}

/// Result of a single enqueue.
#[napi(object)]
pub struct EnqueueOutcome {
    /// `true` if inserted; `false` if skipped due to a `unique_key` conflict.
    pub inserted: bool,
    /// The new job id. Null when skipped.
    pub id: Option<i64>,
}

/// One item in an `enqueueMany` batch. Mixed `kind` is supported across items.
#[napi(object)]
pub struct EnqueueManyItem {
    /// Job kind — matches `@JobHandler(kind)` on the worker side.
    pub kind: String,
    pub payload: serde_json::Value,
    pub max_attempts: Option<i32>,
    pub priority: Option<i16>,
    pub queue: Option<String>,
    /// Run no earlier than this time (epoch milliseconds). Default now.
    /// Mutually exclusive with `delayMs`.
    pub scheduled_at_ms: Option<i64>,
    /// Delay this job by N milliseconds relative to batch-submit time.
    /// Mutually exclusive with `scheduledAtMs`.
    pub delay_ms: Option<i64>,
    pub unique_key: Option<String>,
    pub group_key: Option<String>,
    /// Admin-visible tags (stored on the job row, queryable via `listJobs`).
    pub tags: Option<Vec<String>>,
    pub metadata: Option<serde_json::Value>,
}

/// Aggregate result of `enqueueMany`. Per-job ids are not returned — use
/// single `enqueue` calls when you need them.
#[napi(object)]
pub struct BulkEnqueueOutcome {
    /// Rows newly inserted.
    pub inserted: i64,
    /// Rows skipped due to `unique_key` conflicts.
    pub skipped: i64,
}

/// Input to `enqueueBatch`. `items` is the work; `onComplete` (optional) fires
/// exactly once when every item reaches a terminal state. The handler receives
/// counts under `_eddyq_batch` in its payload: `{ batchId, total, completed,
/// failed, cancelled, durationMs }`.
#[napi(object)]
pub struct EnqueueBatchInput {
    /// Items to enqueue. Mixed `kind` across items is supported. Same 5,000
    /// per-call cap as `enqueueMany`.
    pub items: Vec<EnqueueManyItem>,
    /// Optional callback to enqueue when every item reaches terminal state.
    /// Fires regardless of mix of success / terminal-failure / cancellation;
    /// branch on the counts in the payload's `_eddyq_batch` envelope.
    pub on_complete: Option<EnqueueManyItem>,
    /// Free-form metadata stored on the batch row (admin / dashboard).
    pub metadata: Option<serde_json::Value>,
}

/// Result of `enqueueBatch`. `inserted` is the actual count that became jobs;
/// `skipped` is items that conflicted on `unique_key` (they don't count toward
/// the batch — the batch's `total` is `inserted`).
#[napi(object)]
pub struct BatchEnqueueOutcome {
    pub batch_id: i64,
    pub inserted: i64,
    pub skipped: i64,
}

/// A pending or applied migration.
#[napi(object)]
pub struct MigrationStatus {
    pub version: i64,
    pub name: String,
    /// ISO-8601 timestamp if applied, null if pending.
    pub applied_at: Option<String>,
}

/// Result of migrate / migrate-down.
#[napi(object)]
pub struct MigrateReport {
    pub applied: Vec<MigrationRow>,
    pub rolled_back: Vec<MigrationRow>,
}

#[napi(object)]
pub struct MigrationRow {
    pub version: i64,
    pub name: String,
}

/// Options for `eddyq.start()`. All tuning fields are optional — omit to use
/// the core defaults. Defaults are sensible for most workloads; tune only when
/// you have a measured reason to.
#[napi(object)]
pub struct StartOptions {
    /// Skip the pending-migration check. Default `false` — `start()` errors
    /// out if any registered migration isn't applied, so you never boot
    /// workers against a stale schema.
    ///
    /// Set to `true` only when you've applied migrations via a separate
    /// deploy step (recommended) and don't want the boot-time check.
    pub skip_migration_check: Option<bool>,

    /// How often the heartbeat sweeper reclaims stale running jobs.
    /// Default 30_000 (30s).
    pub sweep_interval_ms: Option<u32>,

    /// A running job is considered stale if its heartbeat is older than this.
    /// The sweeper requeues stale jobs on the next tick. Default 60_000 (60s).
    /// Should be at least 2× `heartbeatIntervalMs`.
    pub stale_after_ms: Option<u32>,

    /// How often each running handler refreshes its lease. Default 15_000 (15s).
    pub heartbeat_interval_ms: Option<u32>,

    /// How often the cleanup task deletes finalized jobs past retention.
    /// Default 300_000 (5m).
    pub cleanup_interval_ms: Option<u32>,

    /// Retention (seconds) for completed jobs. Default 86_400 (24h).
    /// Pass `-1` to keep forever (table grows unbounded).
    pub completed_retention_secs: Option<i64>,

    /// Retention (seconds) for failed jobs. Default 604_800 (7d).
    /// Pass `-1` to keep forever.
    pub failed_retention_secs: Option<i64>,

    /// Retention (seconds) for cancelled jobs. Default 604_800 (7d).
    /// Pass `-1` to keep forever.
    pub cancelled_retention_secs: Option<i64>,

    /// Retention (seconds) for finalized batch rows (`eddyq_batches`).
    /// Default 604_800 (7d). Pass `-1` to keep forever — the table will
    /// grow unbounded for batch-heavy workloads.
    pub batch_retention_secs: Option<i64>,

    /// Leader-election lease in seconds — the elected maintenance node
    /// (scheduler + cleanup) refreshes every `leaseSecs / 3` seconds.
    /// Default 30.
    pub leader_lease_secs: Option<u32>,

    /// Fetch poll interval in poll-only mode (no LISTEN/NOTIFY).
    /// Default 1_000 (1s). Ignored when LISTEN is enabled.
    pub fetch_poll_interval_ms: Option<u32>,

    /// How often the leader scheduler loop fires due schedules + promotes
    /// delayed jobs. Default 5_000 (5s). Lower values make `{ every: ms }`
    /// interval schedules feel more responsive at the cost of more leader
    /// round-trips. Don't push below ~50ms.
    pub scheduler_interval_ms: Option<u32>,
}

// -- Dashboard DTOs ----------------------------------------------------------

/// One bucket of the (queue, state) → count histogram returned by `getStats`.
#[napi(object)]
pub struct QueueStateCount {
    pub queue: String,
    /// One of: `"pending" | "running" | "completed" | "failed" | "scheduled" | "cancelled"`.
    pub state: String,
    pub count: i64,
}

/// Snapshot of job counts grouped by (queue, state). Single SQL round trip.
#[napi(object)]
pub struct JobStats {
    pub by_queue_state: Vec<QueueStateCount>,
}

/// Optional filters for `listJobs`. Active filters AND together.
#[napi(object)]
pub struct ListJobsFilter {
    pub queue: Option<String>,
    /// Restrict to one state. Use `listJobs` without a filter and group
    /// client-side if you need multiple.
    pub state: Option<String>,
    pub kind: Option<String>,
    pub group_key: Option<String>,
    pub tag: Option<String>,
    pub id: Option<i64>,
}

/// Pagination options for `listJobs`. Defaults: limit=50, offset=0. Limit caps at 500.
#[napi(object)]
pub struct Pagination {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// A single job row — enough to render a dashboard table and the job detail
/// drawer. Timestamps are ISO-8601 strings; JSON columns (`payload`, `result`,
/// `errors`, `metadata`) are passed through unchanged.
#[napi(object)]
pub struct JobRow {
    pub id: i64,
    pub queue: String,
    pub kind: String,
    pub state: String,
    pub priority: i16,
    pub attempt: i32,
    pub max_attempts: i32,
    pub scheduled_at: String,
    pub created_at: String,
    pub finalized_at: Option<String>,
    pub group_key: Option<String>,
    pub tags: Vec<String>,
    pub payload: serde_json::Value,
    pub result: Option<serde_json::Value>,
    pub errors: serde_json::Value,
    pub metadata: serde_json::Value,
}

/// Page of jobs + total count (ignores limit/offset) for pagination UIs.
#[napi(object)]
pub struct JobList {
    pub total: i64,
    pub rows: Vec<JobRow>,
}

/// A named queue — cross-process concurrency + pause state + optional default
/// timeout. One row per explicitly-configured queue; queues with no row are
/// implicitly unlimited.
#[napi(object)]
pub struct NamedQueue {
    pub name: String,
    pub running_count: i32,
    pub max_concurrency: i32,
    pub paused: bool,
    pub default_timeout_ms: Option<i32>,
    pub created_at: String,
    pub updated_at: String,
}

/// A group — concurrency cap, pause state, optional token-bucket rate limit.
#[napi(object)]
pub struct Group {
    pub key: String,
    pub running_count: i32,
    pub max_concurrency: i32,
    pub paused: bool,
    pub rate_count: Option<i32>,
    pub rate_period_ms: Option<i32>,
    pub tokens: f64,
    pub tokens_refilled_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// A cron schedule registered via `addSchedule`.
#[napi(object)]
pub struct Schedule {
    pub name: String,
    pub kind: String,
    pub payload: serde_json::Value,
    pub cron_expr: String,
    pub next_run_at: String,
    pub last_run_at: Option<String>,
    pub enabled: bool,
    pub priority: i16,
    pub max_attempts: i32,
    /// Named queue the fired job lands on.
    pub queue: String,
}

/// Options for `addSchedule`. All optional — defaults match `enqueue`.
#[napi(object)]
pub struct ScheduleOptions {
    /// Priority (higher runs first). Default 0.
    pub priority: Option<i16>,
    /// Max total attempts before the job is marked failed. Default 3.
    pub max_attempts: Option<i32>,
    /// Named queue the fired job lands on. Default `"default"`.
    pub queue: Option<String>,
}

/// A single declared schedule in `syncSchedules`. Same shape as `addSchedule`'s
/// arguments, just packaged as an object so the whole declared list can be
/// passed in one call.
#[napi(object)]
pub struct ScheduleDeclaration {
    pub name: String,
    pub cron_expr: String,
    pub kind: String,
    pub payload: serde_json::Value,
    pub priority: Option<i16>,
    pub max_attempts: Option<i32>,
    /// Named queue the fired job lands on. Default `"default"`.
    pub queue: Option<String>,
}

/// Result of `syncSchedules`: the names that were upserted (count) and any
/// schedules that were deleted because they weren't in the declared list.
#[napi(object)]
pub struct SyncSchedulesReport {
    pub upserted: u32,
    pub deleted: Vec<String>,
}

/// Argument passed to a JS worker handler. The payload is whatever JSON was
/// enqueued; the other fields are the `JobContext` flattened so handlers can
/// destructure `{ payload, id, kind, attempt, maxAttempts }`.
#[napi(object)]
pub struct JobCall {
    pub payload: serde_json::Value,
    pub id: i64,
    pub kind: String,
    pub attempt: i32,
    pub max_attempts: i32,
}

/// JS side: `async (call: JobCall) => unknown`. Stored Arc-wrapped so the
/// dispatcher closure (invoked once per job) can cheaply clone a reference
/// for each tsfn call instead of moving ownership.
///
/// `CalleeHandled = false` drops the node-style `(err, arg) => ...` convention
/// so users write `(call) => ...` directly. Trade-off: a **synchronous** throw
/// inside the handler would crash Node — always use `async` handlers (promise
/// rejections are caught by the awaiter on the Rust side and become retries).
type JsTsFn = ThreadsafeFunction<JobCall, Promise<serde_json::Value>, JobCall, napi::Status, false>;
type JsHandler = Arc<JsTsFn>;

/// Abort-broadcast handler: `(reason: string) => void`. Invoked on shutdown
/// so the JS wrapper can fire `.abort()` on all in-flight `AbortController`s.
type JsAbortFn = ThreadsafeFunction<String, (), String, napi::Status, false>;

// -- Queue class -------------------------------------------------------------

/// Connection to eddyq. Owns a Postgres pool; share a single instance across
/// your app and call `close()` on shutdown.
///
/// Exposed to JS as `Eddyq` — the Rust struct stays `Queue` internally.
#[napi(js_name = "Eddyq")]
pub struct Queue {
    client: Client,
    state: Arc<Mutex<WorkerState>>,
    abort_handler: Arc<Mutex<Option<JsAbortFn>>>,
    poll_only: bool,
}

enum WorkerState {
    /// No worker runtime started yet. `work()` appends handlers here; `start()`
    /// consumes them into a running core::Queue.
    Building {
        handlers: Vec<(String, JsHandler)>,
        concurrency: Option<usize>,
        subscribe: Option<Vec<String>>,
    },
    Running {
        queue: Arc<CoreQueue>,
    },
    Stopped,
}

impl Default for WorkerState {
    fn default() -> Self {
        Self::Building {
            handlers: Vec::new(),
            concurrency: None,
            subscribe: None,
        }
    }
}

#[napi]
impl Queue {
    /// Connect to Postgres and construct a client. Does not run migrations —
    /// call `migrate()` on first boot (or on app deploy).
    #[napi(factory)]
    pub async fn connect(database_url: String, options: Option<ConnectOptions>) -> Result<Queue> {
        let poll_only = options.as_ref().and_then(|o| o.poll_only).unwrap_or(false);
        let cfg = build_cfg(options);
        let client =
            run(move || async move { Client::connect_with(&database_url, cfg).await.map_err(err) })
                .await?;
        Ok(Queue {
            client,
            state: Arc::new(Mutex::new(WorkerState::default())),
            abort_handler: Arc::new(Mutex::new(None)),
            poll_only,
        })
    }

    /// The migration line this client was built for (default: `"main"`).
    #[napi(getter)]
    pub fn line(&self) -> &str {
        self.client.line()
    }

    /// Apply all pending schema migrations.
    #[napi]
    pub async fn migrate(&self) -> Result<MigrateReport> {
        let client = self.client.clone();
        run(move || do_migrate(client)).await
    }

    /// Roll back up to `max_steps` migrations.
    #[napi]
    pub async fn migrate_down(&self, max_steps: u32) -> Result<MigrateReport> {
        let client = self.client.clone();
        run(move || do_migrate_down(client, max_steps as usize)).await
    }

    /// Full migration status (all known versions, applied or pending).
    #[napi]
    pub async fn migration_status(&self) -> Result<Vec<MigrationStatus>> {
        let client = self.client.clone();
        run(move || do_migration_status(client)).await
    }

    /// Enqueue a job. `payload` is serialized as JSON and passed to the
    /// worker registered for `kind`.
    #[napi]
    pub async fn enqueue(
        &self,
        kind: String,
        payload: serde_json::Value,
        options: Option<EnqueueOptions>,
    ) -> Result<EnqueueOutcome> {
        let client = self.client.clone();
        run(move || do_enqueue(client, kind, payload, options)).await
    }

    /// Enqueue a batch of jobs in a single round-trip. Uses a Postgres
    /// `UNNEST`-based INSERT, so 1000 jobs cost roughly one statement instead
    /// of one per job. Mixed `kind` within a batch is supported.
    ///
    /// Returns aggregate counts (`inserted`, `skipped`); per-job ids are not
    /// surfaced — use a stable `uniqueKey` per item if you need to correlate
    /// results back to your own domain objects, or fall back to `enqueue()`
    /// when you need the auto-generated id.
    ///
    /// Batch size is capped at 5,000 items per call — split larger workloads
    /// client-side.
    #[napi]
    pub async fn enqueue_many(&self, items: Vec<EnqueueManyItem>) -> Result<BulkEnqueueOutcome> {
        let client = self.client.clone();
        run(move || do_enqueue_many(client, items)).await
    }

    /// Enqueue a batch of jobs and (optionally) a callback that fires when
    /// every item reaches terminal state. Native fan-in primitive — replaces
    /// the per-app counter table workaround for "run X after these N jobs."
    ///
    /// The callback's payload gets a namespaced envelope:
    /// `{ _eddyq_batch: { batchId, total, completed, failed, cancelled,
    /// durationMs }, ...userPayload }`. Handler branches on `failed` / `cancelled`
    /// counts to decide what success vs partial-failure means in its domain.
    ///
    /// Items skipped via `uniqueKey` dedup do not count toward the batch's
    /// `total` — they belong to the batch that originally enqueued them. The
    /// returned `skipped` reports the count for the caller's logging.
    #[napi]
    pub async fn enqueue_batch(&self, input: EnqueueBatchInput) -> Result<BatchEnqueueOutcome> {
        let client = self.client.clone();
        run(move || do_enqueue_batch(client, input)).await
    }

    /// Cancel a pending job. Returns `true` if cancelled, `false` if the job
    /// doesn't exist or is already running / finalized (handlers must
    /// cooperate to stop a running job — eddyq can't abort it for you).
    #[napi]
    pub async fn cancel(&self, id: i64) -> Result<bool> {
        let client = self.client.clone();
        run(move || do_cancel(client, id)).await
    }

    // --- Group admin ------------------------------------------------------

    /// Cap concurrent running jobs in `group_key`. Jobs with
    /// `enqueue(..., { groupKey })` respect this cap across all workers.
    #[napi]
    pub async fn set_group_concurrency(&self, group_key: String, max: i32) -> Result<()> {
        let client = self.client.clone();
        run(move || async move {
            client
                .set_group_concurrency(&group_key, max)
                .await
                .map_err(err)
        })
        .await
    }

    #[napi]
    pub async fn pause_group(&self, group_key: String) -> Result<()> {
        let client = self.client.clone();
        run(move || async move { client.pause_group(&group_key).await.map_err(err) }).await
    }

    #[napi]
    pub async fn resume_group(&self, group_key: String) -> Result<()> {
        let client = self.client.clone();
        run(move || async move { client.resume_group(&group_key).await.map_err(err) }).await
    }

    /// Token-bucket rate limit: at most `count` jobs may *start* per
    /// `periodMs` milliseconds in this group.
    #[napi]
    pub async fn set_group_rate(
        &self,
        group_key: String,
        count: u32,
        period_ms: u32,
    ) -> Result<()> {
        let client = self.client.clone();
        let period = Duration::from_millis(u64::from(period_ms));
        run(move || async move {
            client
                .set_group_rate(&group_key, count, period)
                .await
                .map_err(err)
        })
        .await
    }

    #[napi]
    pub async fn clear_group_rate(&self, group_key: String) -> Result<()> {
        let client = self.client.clone();
        run(move || async move { client.clear_group_rate(&group_key).await.map_err(err) }).await
    }

    // --- Named-queue admin ------------------------------------------------

    /// Cap total running jobs on a named queue across **all worker processes**.
    #[napi]
    pub async fn set_queue_concurrency(&self, queue: String, max: i32) -> Result<()> {
        let client = self.client.clone();
        run(move || async move { client.set_queue_concurrency(&queue, max).await.map_err(err) })
            .await
    }

    #[napi]
    pub async fn pause_queue(&self, queue: String) -> Result<()> {
        let client = self.client.clone();
        run(move || async move { client.pause_queue(&queue).await.map_err(err) }).await
    }

    #[napi]
    pub async fn resume_queue(&self, queue: String) -> Result<()> {
        let client = self.client.clone();
        run(move || async move { client.resume_queue(&queue).await.map_err(err) }).await
    }

    /// Set a default per-job timeout (milliseconds) for this named queue.
    /// Pass `null` to clear.
    #[napi]
    pub async fn set_queue_timeout(&self, queue: String, timeout_ms: Option<u32>) -> Result<()> {
        let client = self.client.clone();
        let timeout = timeout_ms.map(|ms| Duration::from_millis(u64::from(ms)));
        run(move || async move { client.set_queue_timeout(&queue, timeout).await.map_err(err) })
            .await
    }

    // --- Dashboard / list queries -----------------------------------------

    /// Job counts grouped by (queue, state). One SQL query — use as the
    /// landing query for a dashboard.
    #[napi]
    pub async fn get_stats(&self) -> Result<JobStats> {
        let client = self.client.clone();
        run(move || do_get_stats(client)).await
    }

    /// Paginated job listing with optional filters. Defaults: limit=50,
    /// offset=0. Limit caps at 500.
    #[napi]
    pub async fn list_jobs(
        &self,
        filter: Option<ListJobsFilter>,
        pagination: Option<Pagination>,
    ) -> Result<JobList> {
        let client = self.client.clone();
        run(move || do_list_jobs(client, filter, pagination)).await
    }

    /// Every named queue that has an explicit row (concurrency cap, pause
    /// state, etc.). Queues with no row are implicitly unlimited and not
    /// returned here — use `getStats()` to see all queues with live jobs.
    #[napi]
    pub async fn list_named_queues(&self) -> Result<Vec<NamedQueue>> {
        let client = self.client.clone();
        run(move || do_list_named_queues(client)).await
    }

    /// Every group that has an explicit row (cap, pause, rate-limit state).
    #[napi]
    pub async fn list_groups(&self) -> Result<Vec<Group>> {
        let client = self.client.clone();
        run(move || do_list_groups(client)).await
    }

    /// Every registered cron schedule.
    #[napi]
    pub async fn list_schedules(&self) -> Result<Vec<Schedule>> {
        let client = self.client.clone();
        run(move || do_list_schedules(client)).await
    }

    /// Upsert a cron schedule. Jobs of `kind` with the given `payload` will
    /// be enqueued automatically each time the cron fires. Passing the same
    /// `name` updates the schedule in place.
    ///
    /// Cron syntax is a 6- or 7-field `sec min hour day month dayOfWeek [year]`
    /// expression (the `cron` crate's dialect — note the leading seconds field).
    ///
    /// ```ts
    /// await queue.addSchedule(
    ///   "daily-report",
    ///   "0 0 8 * * *",           // every day at 08:00:00 UTC
    ///   "report.generate",
    ///   { scope: "daily" },
    ///   { priority: 5 },
    /// );
    /// ```
    #[napi]
    pub async fn add_schedule(
        &self,
        name: String,
        cron_expr: String,
        kind: String,
        payload: serde_json::Value,
        options: Option<ScheduleOptions>,
    ) -> Result<()> {
        let client = self.client.clone();
        let priority = options.as_ref().and_then(|o| o.priority).unwrap_or(0);
        let max_attempts = options.as_ref().and_then(|o| o.max_attempts).unwrap_or(3);
        let queue = options
            .and_then(|o| o.queue)
            .unwrap_or_else(|| eddyq_client::DEFAULT_QUEUE.to_string());
        run(move || async move {
            client
                .add_schedule(
                    &name,
                    &cron_expr,
                    &kind,
                    payload,
                    priority,
                    max_attempts,
                    &queue,
                )
                .await
                .map_err(err)
        })
        .await
    }

    /// Remove a schedule. Returns `true` if a row was deleted.
    #[napi]
    pub async fn remove_schedule(&self, name: String) -> Result<bool> {
        let client = self.client.clone();
        run(move || async move { client.remove_schedule(&name).await.map_err(err) }).await
    }

    /// Reconcile DB schedules against a code-declared list. Each entry is
    /// upserted; any DB schedule whose name is not in `declared` is deleted.
    /// Idempotent — safe to run on every boot. Use this when schedules are
    /// declared in module config (e.g. `EddyqModule.forRoot({ schedules })`).
    #[napi]
    pub async fn sync_schedules(
        &self,
        declared: Vec<ScheduleDeclaration>,
    ) -> Result<SyncSchedulesReport> {
        let client = self.client.clone();
        let mapped: Vec<CoreScheduleDeclaration> = declared
            .into_iter()
            .map(|d| CoreScheduleDeclaration {
                name: d.name,
                cron_expr: d.cron_expr,
                kind: d.kind,
                payload: d.payload,
                priority: d.priority.unwrap_or(0),
                max_attempts: d.max_attempts.unwrap_or(3),
                queue: d
                    .queue
                    .unwrap_or_else(|| eddyq_client::DEFAULT_QUEUE.to_string()),
            })
            .collect();
        run(move || async move {
            let report = client.sync_schedules(&mapped).await.map_err(err)?;
            Ok(SyncSchedulesReport {
                upserted: report.upserted as u32,
                deleted: report.deleted,
            })
        })
        .await
    }

    /// Toggle a schedule on or off without deleting it. Returns `true` if a
    /// row was updated.
    #[napi]
    pub async fn set_schedule_enabled(&self, name: String, enabled: bool) -> Result<bool> {
        let client = self.client.clone();
        run(move || async move {
            client
                .set_schedule_enabled(&name, enabled)
                .await
                .map_err(err)
        })
        .await
    }

    // --- Worker registration ----------------------------------------------

    /// Register an async JS handler for a job kind. Call once per kind before
    /// `start()`. The handler receives a `JobCall` object and should resolve
    /// on success or throw/reject to trigger a retry.
    ///
    /// ```ts
    /// await queue.work("send.email", async ({ payload, id, attempt }) => {
    ///   await sendgrid.send(payload);
    /// });
    /// ```
    #[napi(ts_args_type = "kind: string, handler: (call: JobCall) => Promise<unknown>")]
    pub fn work(&self, kind: String, handler: JsTsFn) -> Result<()> {
        self.register_handler_arc(kind, Arc::new(handler))
    }

    /// Rust-only handler-registration path. Takes an `Arc<JsTsFn>` so a
    /// shared threadsafe-function can fan out to multiple backends (used
    /// by `EddyqApp`). Not exposed to NAPI (no `#[napi]` attribute).
    pub(crate) fn register_handler_arc(&self, kind: String, handler: JsHandler) -> Result<()> {
        let mut state = self.state.lock().expect("worker state lock poisoned");
        match &mut *state {
            WorkerState::Building { handlers, .. } => {
                handlers.push((kind, handler));
                Ok(())
            }
            WorkerState::Running { .. } => Err(napi::Error::from_reason(
                "Queue is already running — register handlers before calling start()",
            )),
            WorkerState::Stopped => Err(napi::Error::from_reason(
                "Queue has been shut down and cannot be reused",
            )),
        }
    }

    /// Set worker concurrency (max in-flight jobs in this process). Default 10.
    /// Must be called before `start()`.
    #[napi]
    pub fn set_worker_concurrency(&self, n: u32) -> Result<()> {
        let mut state = self.state.lock().expect("worker state lock poisoned");
        match &mut *state {
            WorkerState::Building { concurrency, .. } => {
                *concurrency = Some(n.max(1) as usize);
                Ok(())
            }
            _ => Err(napi::Error::from_reason(
                "Cannot change concurrency after start()",
            )),
        }
    }

    /// Subscribe this worker to specific named queues. Default `["default"]`.
    /// Must be called before `start()`.
    #[napi]
    pub fn subscribe_to(&self, queues: Vec<String>) -> Result<()> {
        let mut state = self.state.lock().expect("worker state lock poisoned");
        match &mut *state {
            WorkerState::Building { subscribe, .. } => {
                *subscribe = Some(queues);
                Ok(())
            }
            _ => Err(napi::Error::from_reason(
                "Cannot change queue subscriptions after start()",
            )),
        }
    }

    /// Start the worker runtime. Handlers registered via `work()` begin
    /// processing jobs from Postgres. Fetch/sweep/scheduler loops run until
    /// `shutdown()` is called.
    ///
    /// **Pending-migration guard.** Before starting, checks that every
    /// migration the binary knows about has been applied. If any are missing,
    /// `start()` errors out with a clear message instead of booting workers
    /// that will trip on missing columns. Pass `{ skipMigrationCheck: true }`
    /// to override (e.g. when schema is managed by a separate deploy step).
    ///
    /// Async so the internal `tokio::spawn`s land on napi's tokio runtime.
    #[napi]
    pub async fn start(&self, options: Option<StartOptions>) -> Result<()> {
        let (handlers, concurrency, subscribe) = {
            let mut state = self.state.lock().expect("worker state lock poisoned");
            match std::mem::replace(&mut *state, WorkerState::Stopped) {
                WorkerState::Building {
                    handlers,
                    concurrency,
                    subscribe,
                } => (handlers, concurrency, subscribe),
                WorkerState::Running { queue } => {
                    *state = WorkerState::Running { queue };
                    return Err(napi::Error::from_reason("Queue is already running"));
                }
                WorkerState::Stopped => {
                    return Err(napi::Error::from_reason(
                        "Queue has been shut down and cannot be reused",
                    ));
                }
            }
        };

        if handlers.is_empty() {
            // Restore state so start() can be called again after registering handlers.
            let mut state = self.state.lock().expect("worker state lock poisoned");
            *state = WorkerState::Building {
                handlers: Vec::new(),
                concurrency,
                subscribe,
            };
            return Err(napi::Error::from_reason(
                "No handlers registered — call work(kind, fn) before start()",
            ));
        }

        // Pending-migration guard. Migrations are a DEPLOY-STEP concern — we
        // intentionally don't auto-apply at boot because a slow migration
        // would block app startup for every replica. If anything's pending,
        // we refuse to start and tell
        // the operator how to fix it.
        let skip_check = options
            .as_ref()
            .and_then(|o| o.skip_migration_check)
            .unwrap_or(false);
        if !skip_check {
            let client = self.client.clone();
            let statuses =
                run(move || async move { client.migration_status().await.map_err(err) }).await?;
            let pending: Vec<_> = statuses.iter().filter(|s| s.applied_at.is_none()).collect();
            if !pending.is_empty() {
                let names: Vec<String> = pending
                    .iter()
                    .map(|p| format!("{}:{}", p.version, p.name))
                    .collect();
                return Err(napi::Error::from_reason(format!(
                    "eddyq: {} pending migration(s) — will not start workers against stale schema.\n\
                     Pending: {}.\n\n\
                     Apply them as a deploy step BEFORE booting workers:\n  \
                     • CLI:  `eddyq migrate run --database-url $DATABASE_URL`\n  \
                     • Or a one-shot Node script: `await Eddyq.connect(url).then(q => q.migrate())`\n\n\
                     Once applied, restart workers. If you've already migrated out-of-band and want \
                     to silence this check, pass `{{ skipMigrationCheck: true }}` to start().",
                    pending.len(),
                    names.join(", ")
                )));
            }
        }

        let mut builder = CoreQueueBuilder::new(self.client.pool().clone())
            .line(self.client.line())
            .poll_only(self.poll_only);
        if let Some(n) = concurrency {
            builder = builder.worker_concurrency(n);
        }
        if let Some(qs) = subscribe {
            builder = builder.subscribe_to(qs);
        }
        if let Some(o) = options.as_ref() {
            if let Some(ms) = o.sweep_interval_ms {
                builder = builder.sweep_interval(Duration::from_millis(u64::from(ms)));
            }
            if let Some(ms) = o.stale_after_ms {
                builder = builder.stale_after(Duration::from_millis(u64::from(ms)));
            }
            if let Some(ms) = o.heartbeat_interval_ms {
                builder = builder.heartbeat_interval(Duration::from_millis(u64::from(ms)));
            }
            if let Some(ms) = o.cleanup_interval_ms {
                builder = builder.cleanup_interval(Duration::from_millis(u64::from(ms)));
            }
            if let Some(secs) = o.completed_retention_secs {
                builder = builder.completed_retention(retention_from_secs(secs));
            }
            if let Some(secs) = o.failed_retention_secs {
                builder = builder.failed_retention(retention_from_secs(secs));
            }
            if let Some(secs) = o.cancelled_retention_secs {
                builder = builder.cancelled_retention(retention_from_secs(secs));
            }
            if let Some(secs) = o.batch_retention_secs {
                builder = builder.batch_retention(retention_from_secs(secs));
            }
            if let Some(s) = o.leader_lease_secs {
                builder = builder.leader_lease_secs(u64::from(s));
            }
            if let Some(ms) = o.fetch_poll_interval_ms {
                builder = builder.fetch_poll_interval(Duration::from_millis(u64::from(ms)));
            }
            if let Some(ms) = o.scheduler_interval_ms {
                builder = builder.scheduler_interval(Duration::from_millis(u64::from(ms)));
            }
        }
        for (kind, tsfn) in handlers {
            builder = builder.register_dyn(kind, dispatcher(tsfn));
        }

        let queue = Arc::new(builder.build());
        queue.start().map_err(err)?;

        let mut state = self.state.lock().expect("worker state lock poisoned");
        *state = WorkerState::Running { queue };
        Ok(())
    }

    /// Register a handler invoked when `shutdown()` is called. The handler's
    /// `reason` arg is a human-readable string; the JS ergonomics layer
    /// (lib.cjs) uses this to broadcast `.abort()` to all in-flight
    /// `AbortController`s, so user handlers observing `call.signal` can bail.
    ///
    /// Most users don't call this directly — lib.cjs wires it automatically.
    #[napi(ts_args_type = "handler: (reason: string) => void")]
    pub fn set_abort_handler(&self, handler: JsAbortFn) -> Result<()> {
        let mut slot = self
            .abort_handler
            .lock()
            .expect("abort handler lock poisoned");
        *slot = Some(handler);
        Ok(())
    }

    /// Stop the worker runtime. Signals any registered abort handler first,
    /// then waits up to `gracefulTimeoutMs` (default 30 000) for in-flight
    /// jobs to finish before forcibly cancelling the runtime tasks. Admin
    /// methods remain usable after shutdown — call `close()` to release the
    /// DB pool entirely.
    ///
    /// On return, all NAPI `ThreadsafeFunction` references this binding holds
    /// are dropped (handler TSFNs via the worker registry, plus the abort
    /// TSFN). That releases their libuv ref counts so Node's event loop can
    /// drain naturally — without that drop, a Nest `app.close()` on SIGTERM
    /// would call this method but the process would still hang until the
    /// orchestrator force-killed it.
    ///
    /// `options.mode`:
    ///   - `"drain"` (default) — graceful: stop claiming new jobs, fire
    ///     `AbortSignal` to in-flight handlers, await up to
    ///     `gracefulTimeoutMs`. Use for routine deploys.
    ///   - `"force"` — fast: abort runtime tasks immediately and proactively
    ///     reclaim rows this pod was processing (set running→pending) so
    ///     other pods pick them up without waiting for heartbeat sweep.
    ///     Use when SIGKILL is imminent (Kubernetes grace period almost
    ///     up). Modeled on BullMQ's `worker.close({ force: true })`.
    ///   - `"abandon"` — last-resort: drop runtime, leave rows alone. The
    ///     heartbeat sweep on another pod will recover after `staleAfter`.
    ///     Use only on panic exits.
    #[napi]
    pub async fn shutdown(&self, options: Option<ShutdownOptions>) -> Result<()> {
        let queue = {
            let mut state = self.state.lock().expect("worker state lock poisoned");
            match std::mem::replace(&mut *state, WorkerState::Stopped) {
                WorkerState::Running { queue } => queue,
                WorkerState::Building { .. } | WorkerState::Stopped => {
                    return Err(napi::Error::from_reason("Queue is not running"));
                }
            }
        };

        let mode = options
            .as_ref()
            .and_then(|o| o.mode.as_deref())
            .unwrap_or("drain");
        let core_mode = match mode {
            "drain" => ShutdownMode::Drain,
            "force" => ShutdownMode::Force,
            "abandon" => ShutdownMode::Abandon,
            other => {
                return Err(napi::Error::from_reason(format!(
                    "shutdown: invalid mode {other:?} (expected \"drain\" | \"force\" | \"abandon\")"
                )));
            }
        };

        // Drain: fire abort *first* so handlers receive `signal.aborted`
        // during the graceful-wait window and can wind down cooperatively.
        //
        // Force/Abandon: fire abort *after* core shutdown. The Force path
        // needs to snapshot the `in_flight` set before any handler resolves,
        // and the abort-callback path can race with that snapshot through
        // the spawn_blocking yield (JS resolves handler → mark_completed →
        // in_flight.remove → snapshot finds an empty set). Firing abort
        // after shutdown_with returns is correct because:
        //   - Force: runtime tasks are already aborted; JS handlers
        //     resolving has nowhere to go on the Rust side, but JS still
        //     gets `signal.aborted` so its event loop drains.
        //   - Abandon: same — handlers wake up, resolve, JS exits cleanly.
        //
        // The TSFN is left on `self.abort_handler` for `close()` to drop.
        let fire_abort_now = || {
            let guard = self
                .abort_handler
                .lock()
                .expect("abort handler lock poisoned");
            if let Some(handler) = guard.as_ref() {
                handler.call(
                    "shutdown".to_owned(),
                    ThreadsafeFunctionCallMode::NonBlocking,
                );
            }
        };

        if core_mode == ShutdownMode::Drain {
            fire_abort_now();
            let grace = Duration::from_millis(u64::from(
                options
                    .as_ref()
                    .and_then(|o| o.graceful_timeout_ms)
                    .unwrap_or(30_000),
            ));
            let fut = run(move || async move { queue.shutdown_with(core_mode).await.map_err(err) });
            match tokio::time::timeout(grace, fut).await {
                Ok(res) => res,
                Err(_) => Err(napi::Error::from_reason(format!(
                    "shutdown exceeded graceful timeout ({grace:?}) — runtime tasks still in flight. \
                     Re-run with mode=\"force\" if jobs need to be made re-eligible immediately."
                ))),
            }
        } else {
            // Force / Abandon both bound their own work and don't honor a
            // user-supplied timeout — they're already fast paths.
            let result =
                run(move || async move { queue.shutdown_with(core_mode).await.map_err(err) }).await;
            fire_abort_now();
            result
        }
    }

    // --- Lifecycle --------------------------------------------------------

    /// Close the underlying Postgres pool and release any retained NAPI
    /// `ThreadsafeFunction`s. Call on shutdown.
    ///
    /// Defensive in two ways:
    ///   - If `shutdown()` was never called and the queue is `Running`, we
    ///     run an internal `Abandon` shutdown (drops runtime without
    ///     awaiting handlers, leaves DB rows for heartbeat-sweep recovery).
    ///     This isn't graceful — callers should `await queue.shutdown()`
    ///     first — but it guarantees `close()` can't hang the process.
    ///   - The abort TSFN is dropped if `shutdown()` didn't already.
    ///
    /// After `close()`, no further calls on this `Eddyq` instance are valid.
    #[napi]
    pub async fn close(&self) -> Result<()> {
        // If the runtime is still running, abandon it so we don't strand
        // the process. Take the queue out of WorkerState and abandon-shutdown
        // it before swapping to Stopped — this also drops the handler TSFNs
        // (held inside the runtime's worker registry) deterministically.
        let to_abandon = {
            let mut state = self.state.lock().expect("worker state lock poisoned");
            match std::mem::replace(&mut *state, WorkerState::Stopped) {
                WorkerState::Running { queue } => Some(queue),
                WorkerState::Building { .. } | WorkerState::Stopped => None,
            }
        };
        if let Some(queue) = to_abandon {
            // Best-effort. If it errors (e.g. NotRunning race), nothing left
            // to do — we've already swapped state to Stopped.
            let _ = run(move || async move {
                queue
                    .shutdown_with(ShutdownMode::Abandon)
                    .await
                    .map_err(err)
            })
            .await;
        }

        // And the abort TSFN if shutdown() didn't already drop it.
        {
            let mut guard = self
                .abort_handler
                .lock()
                .expect("abort handler lock poisoned");
            let _ = guard.take();
        }

        let client = self.client.clone();
        run(move || async move {
            client.close().await;
            Ok(())
        })
        .await
    }
}

/// Build the `Handler` closure that bridges eddyq-core's dispatch to a JS
/// ThreadsafeFunction. Each call: (a) hops to the Node main thread with the
/// `JobCall`, (b) receives the Promise the handler returned, (c) awaits the
/// Promise's resolved value. A rejection is surfaced as a JobResult error so
/// the core runtime's retry/fail logic kicks in.
fn dispatcher(
    tsfn: JsHandler,
) -> impl Fn(
    serde_json::Value,
    JobContext,
)
    -> std::pin::Pin<Box<dyn std::future::Future<Output = JobResult<serde_json::Value>> + Send>>
+ Send
+ Sync
+ 'static {
    move |payload, ctx| {
        let tsfn = tsfn.clone();
        Box::pin(async move {
            let call = JobCall {
                payload,
                id: ctx.id,
                kind: ctx.kind,
                attempt: ctx.attempt,
                max_attempts: ctx.max_attempts,
            };
            let promise = tsfn.call_async(call).await.map_err(|e| {
                // This path means "the JS host couldn't even deliver the call"
                // (process dying, tsfn released). Synthesize a structured
                // failure so the logs / eddyq_jobs.errors rows stay consistent.
                anyhow::Error::from(HandlerFailure {
                    message: format!("threadsafe call failed: {e}"),
                    name: Some("EddyqHostError".into()),
                    ..Default::default()
                })
            })?;
            match promise.await {
                Ok(value) => Ok(value),
                Err(e) => Err(anyhow::Error::from(parse_js_failure(&e))),
            }
        })
    }
}

/// Turn a NAPI-surfaced promise rejection into a structured `HandlerFailure`.
///
/// The JS-side wrapper (`packages/queue/lib.cjs`) catches thrown errors and
/// re-throws an `Error` whose message is prefixed with `[eddyq:err]` followed
/// by a JSON envelope: `{ name, message, stack, directive?, delayMs? }`. Here
/// we pattern-match that envelope; anything else falls through to a plain
/// message-only failure.
fn parse_js_failure(e: &napi::Error) -> HandlerFailure {
    const PREFIX: &str = "[eddyq:err]";
    let reason = e.reason.as_str();
    // The reason may be wrapped by napi and/or the JS `Error.toString()` as
    // `"<Status>, Error: <message>"` or just `"Error: <message>"`. Rather than
    // try to match every combination, locate our envelope marker anywhere in
    // the string and JSON-parse the tail.
    if let Some(idx) = reason.find(PREFIX) {
        let json = &reason[idx + PREFIX.len()..];
        if let Ok(env) = serde_json::from_str::<JsErrorEnvelope>(json) {
            return HandlerFailure {
                message: env.message,
                name: env.name,
                stack: env.stack,
                directive: match env.directive.as_deref() {
                    Some("cancel") => Some(Directive::Cancel),
                    Some("retry") => Some(Directive::Retry {
                        delay_ms: env.delay_ms.unwrap_or(0),
                    }),
                    _ => None,
                },
            };
        }
    }
    // No envelope — fall back to the raw reason (stripped of "GenericFailure, "
    // if present, for cleaner logs).
    let bare = reason.strip_prefix("GenericFailure, ").unwrap_or(reason);
    HandlerFailure::from_message(bare.to_owned())
}

#[derive(serde::Deserialize)]
struct JsErrorEnvelope {
    message: String,
    name: Option<String>,
    stack: Option<String>,
    directive: Option<String>,
    #[serde(rename = "delayMs")]
    delay_ms: Option<u64>,
}

// -- Standalone async helpers: keep #[napi] methods free of deep sqlx futures
//    so rustc's HRTB inference doesn't choke on the macro-generated wrappers.

async fn do_migrate(client: Client) -> Result<MigrateReport> {
    let report = client.migrate().await.map_err(err)?;
    Ok(report_to_dto(&report))
}

async fn do_migrate_down(client: Client, steps: usize) -> Result<MigrateReport> {
    let report = client.migrate_down(steps).await.map_err(err)?;
    Ok(report_to_dto(&report))
}

async fn do_migration_status(client: Client) -> Result<Vec<MigrationStatus>> {
    let rows = client.migration_status().await.map_err(err)?;
    let mut out = Vec::with_capacity(rows.len());
    for s in rows {
        out.push(MigrationStatus {
            version: s.version,
            name: s.name.to_owned(),
            applied_at: s.applied_at.map(|t| t.to_rfc3339()),
        });
    }
    Ok(out)
}

async fn do_enqueue(
    client: Client,
    kind: String,
    payload: serde_json::Value,
    options: Option<EnqueueOptions>,
) -> Result<EnqueueOutcome> {
    let mut req = DynEnqueue::new(kind, payload);
    if let Some(opts) = options {
        if let Some(n) = opts.max_attempts {
            req.max_attempts = n;
        }
        if let Some(p) = opts.priority {
            req.priority = p;
        }
        if let Some(q) = opts.queue {
            req.queue = q;
        }
        if opts.scheduled_at_ms.is_some() && opts.delay_ms.is_some() {
            return Err(napi::Error::from_reason(
                "enqueue: pass either scheduledAtMs or delayMs, not both",
            ));
        }
        if let Some(ms) = opts.scheduled_at_ms {
            req.scheduled_at = Some(ms_to_utc(ms));
        }
        if let Some(ms) = opts.delay_ms {
            req.scheduled_at = Some(Utc::now() + chrono::Duration::milliseconds(ms));
        }
        if let Some(k) = opts.unique_key {
            req.unique_key = Some(k);
        }
        if let Some(g) = opts.group_key {
            req.group_key = Some(g);
        }
        if let Some(t) = opts.tags {
            req.tags = t;
        }
        if let Some(m) = opts.metadata {
            req.metadata = m;
        }
    }
    let result = client.enqueue(req).await.map_err(err)?;
    Ok(match result {
        eddyq_client::EnqueueResult::Inserted(id) => EnqueueOutcome {
            inserted: true,
            id: Some(id),
        },
        eddyq_client::EnqueueResult::Skipped => EnqueueOutcome {
            inserted: false,
            id: None,
        },
    })
}

/// Cap on items per `enqueueMany` call. Each item binds ~10 Postgres
/// parameters and the protocol limit is 65,535 — plus giant batches hold
/// a pool connection for a long time. Split larger workloads client-side.
const ENQUEUE_MANY_MAX: usize = 5_000;

async fn do_enqueue_many(
    client: Client,
    items: Vec<EnqueueManyItem>,
) -> Result<BulkEnqueueOutcome> {
    if items.len() > ENQUEUE_MANY_MAX {
        return Err(napi::Error::from_reason(format!(
            "enqueueMany: batch of {} exceeds max of {}; split client-side",
            items.len(),
            ENQUEUE_MANY_MAX,
        )));
    }
    let mut reqs: Vec<DynEnqueue> = Vec::with_capacity(items.len());
    for item in items {
        if item.scheduled_at_ms.is_some() && item.delay_ms.is_some() {
            return Err(napi::Error::from_reason(
                "enqueueMany: each item must set either scheduledAtMs or delayMs, not both",
            ));
        }
        let mut req = DynEnqueue::new(item.kind, item.payload);
        if let Some(n) = item.max_attempts {
            req.max_attempts = n;
        }
        if let Some(p) = item.priority {
            req.priority = p;
        }
        if let Some(q) = item.queue {
            req.queue = q;
        }
        if let Some(ms) = item.scheduled_at_ms {
            req.scheduled_at = Some(ms_to_utc(ms));
        }
        if let Some(ms) = item.delay_ms {
            req.scheduled_at = Some(Utc::now() + chrono::Duration::milliseconds(ms));
        }
        if let Some(k) = item.unique_key {
            req.unique_key = Some(k);
        }
        if let Some(g) = item.group_key {
            req.group_key = Some(g);
        }
        if let Some(t) = item.tags {
            req.tags = t;
        }
        if let Some(m) = item.metadata {
            req.metadata = m;
        }
        reqs.push(req);
    }
    let result = client.enqueue_many(reqs).await.map_err(err)?;
    Ok(BulkEnqueueOutcome {
        inserted: result.inserted as i64,
        skipped: result.skipped as i64,
    })
}

async fn do_cancel(client: Client, id: i64) -> Result<bool> {
    client.cancel(id).await.map_err(err)
}

fn item_to_dyn(item: EnqueueManyItem) -> Result<eddyq_client::DynEnqueue> {
    if item.scheduled_at_ms.is_some() && item.delay_ms.is_some() {
        return Err(napi::Error::from_reason(
            "each item must set either scheduledAtMs or delayMs, not both",
        ));
    }
    let mut req = eddyq_client::DynEnqueue::new(item.kind, item.payload);
    if let Some(n) = item.max_attempts {
        req.max_attempts = n;
    }
    if let Some(p) = item.priority {
        req.priority = p;
    }
    if let Some(q) = item.queue {
        req.queue = q;
    }
    if let Some(ms) = item.scheduled_at_ms {
        req.scheduled_at = Some(ms_to_utc(ms));
    }
    if let Some(ms) = item.delay_ms {
        req.scheduled_at = Some(Utc::now() + chrono::Duration::milliseconds(ms));
    }
    if let Some(k) = item.unique_key {
        req.unique_key = Some(k);
    }
    if let Some(g) = item.group_key {
        req.group_key = Some(g);
    }
    if let Some(t) = item.tags {
        req.tags = t;
    }
    if let Some(m) = item.metadata {
        req.metadata = m;
    }
    Ok(req)
}

async fn do_enqueue_batch(client: Client, input: EnqueueBatchInput) -> Result<BatchEnqueueOutcome> {
    if input.items.len() > ENQUEUE_MANY_MAX {
        return Err(napi::Error::from_reason(format!(
            "enqueueBatch: batch of {} exceeds max of {}; split client-side",
            input.items.len(),
            ENQUEUE_MANY_MAX,
        )));
    }
    let mut reqs: Vec<eddyq_client::DynEnqueue> = Vec::with_capacity(input.items.len());
    for item in input.items {
        reqs.push(item_to_dyn(item)?);
    }
    let on_complete = match input.on_complete {
        Some(item) => Some(item_to_dyn(item)?),
        None => None,
    };
    let metadata = input.metadata.unwrap_or(serde_json::Value::Null);
    let opts = eddyq_client::BatchOptions {
        on_complete,
        metadata,
    };
    let result = client.enqueue_batch(reqs, opts).await.map_err(err)?;
    Ok(BatchEnqueueOutcome {
        batch_id: result.batch_id,
        inserted: result.inserted as i64,
        skipped: result.skipped as i64,
    })
}

async fn do_get_stats(client: Client) -> Result<JobStats> {
    let stats = client.get_stats().await.map_err(err)?;
    let by_queue_state = stats
        .by_queue_state
        .into_iter()
        .map(|c| QueueStateCount {
            queue: c.queue,
            state: c.state.as_str().to_owned(),
            count: c.count,
        })
        .collect();
    Ok(JobStats { by_queue_state })
}

async fn do_list_jobs(
    client: Client,
    filter: Option<ListJobsFilter>,
    pagination: Option<Pagination>,
) -> Result<JobList> {
    let filter = filter.unwrap_or(ListJobsFilter {
        queue: None,
        state: None,
        kind: None,
        group_key: None,
        tag: None,
        id: None,
    });
    let state = match filter.state.as_deref() {
        None => None,
        Some(s) => Some(parse_job_state(s)?),
    };
    let core_filter = eddyq_client::ListJobsFilter {
        queue: filter.queue,
        state,
        kind: filter.kind,
        group_key: filter.group_key,
        tag: filter.tag,
        id: filter.id,
    };
    let core_pag = match pagination {
        Some(p) => eddyq_client::Pagination {
            limit: p.limit.unwrap_or(50),
            offset: p.offset.unwrap_or(0),
        },
        None => eddyq_client::Pagination::default(),
    };
    let list = client.list_jobs(core_filter, core_pag).await.map_err(err)?;
    Ok(JobList {
        total: list.total,
        rows: list
            .rows
            .into_iter()
            .map(|r| JobRow {
                id: r.id,
                queue: r.queue,
                kind: r.kind,
                state: r.state,
                priority: r.priority,
                attempt: r.attempt,
                max_attempts: r.max_attempts,
                scheduled_at: r.scheduled_at.to_rfc3339(),
                created_at: r.created_at.to_rfc3339(),
                finalized_at: r.finalized_at.map(|t| t.to_rfc3339()),
                group_key: r.group_key,
                tags: r.tags,
                payload: r.payload,
                result: r.result,
                errors: r.errors,
                metadata: r.metadata,
            })
            .collect(),
    })
}

fn parse_job_state(s: &str) -> Result<JobState> {
    match s {
        "pending" => Ok(JobState::Pending),
        "running" => Ok(JobState::Running),
        "completed" => Ok(JobState::Completed),
        "failed" => Ok(JobState::Failed),
        "scheduled" => Ok(JobState::Scheduled),
        "cancelled" => Ok(JobState::Cancelled),
        other => Err(napi::Error::from_reason(format!(
            "invalid state filter: {other:?} — must be one of pending, running, completed, failed, scheduled, cancelled"
        ))),
    }
}

async fn do_list_named_queues(client: Client) -> Result<Vec<NamedQueue>> {
    let rows = client.list_named_queues().await.map_err(err)?;
    Ok(rows
        .into_iter()
        .map(|q| NamedQueue {
            name: q.name,
            running_count: q.running_count,
            max_concurrency: q.max_concurrency,
            paused: q.paused,
            default_timeout_ms: q.default_timeout_ms,
            created_at: q.created_at.to_rfc3339(),
            updated_at: q.updated_at.to_rfc3339(),
        })
        .collect())
}

async fn do_list_groups(client: Client) -> Result<Vec<Group>> {
    let rows = client.list_groups().await.map_err(err)?;
    Ok(rows
        .into_iter()
        .map(|g| Group {
            key: g.key,
            running_count: g.running_count,
            max_concurrency: g.max_concurrency,
            paused: g.paused,
            rate_count: g.rate_count,
            rate_period_ms: g.rate_period_ms,
            tokens: g.tokens,
            tokens_refilled_at: g.tokens_refilled_at.map(|t| t.to_rfc3339()),
            created_at: g.created_at.to_rfc3339(),
            updated_at: g.updated_at.to_rfc3339(),
        })
        .collect())
}

async fn do_list_schedules(client: Client) -> Result<Vec<Schedule>> {
    let rows = client.list_schedules().await.map_err(err)?;
    Ok(rows
        .into_iter()
        .map(|s| Schedule {
            name: s.name,
            kind: s.kind,
            payload: s.payload,
            cron_expr: s.cron_expr,
            next_run_at: s.next_run_at.to_rfc3339(),
            last_run_at: s.last_run_at.map(|t| t.to_rfc3339()),
            enabled: s.enabled,
            priority: s.priority,
            max_attempts: s.max_attempts,
            queue: s.queue,
        })
        .collect())
}

// -- Plain helpers -----------------------------------------------------------

fn build_cfg(options: Option<ConnectOptions>) -> ClientConfig {
    let mut c = ClientConfig::default();
    let Some(o) = options else { return c };
    if let Some(n) = o.max_connections {
        c.max_connections = n;
    }
    if let Some(n) = o.min_connections {
        c.min_connections = n;
    }
    if let Some(ms) = o.acquire_timeout_ms {
        c.acquire_timeout = Duration::from_millis(u64::from(ms));
    }
    if let Some(line) = o.line {
        c.line = line;
    }
    c
}

/// Map a JS-supplied retention value to the core's `Option<Duration>`.
/// Convention: `< 0` → keep forever (`None`); `>= 0` → that many seconds.
fn retention_from_secs(secs: i64) -> Option<Duration> {
    if secs < 0 {
        None
    } else {
        Some(Duration::from_secs(secs as u64))
    }
}

fn ms_to_utc(ms: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(ms)
        .single()
        .unwrap_or_else(Utc::now)
}

fn report_to_dto(report: &eddyq_client::MigrateReport) -> MigrateReport {
    MigrateReport {
        applied: report
            .applied
            .iter()
            .map(|(v, n)| MigrationRow {
                version: *v,
                name: (*n).to_owned(),
            })
            .collect(),
        rolled_back: report
            .rolled_back
            .iter()
            .map(|(v, n)| MigrationRow {
                version: *v,
                name: (*n).to_owned(),
            })
            .collect(),
    }
}

// ============================================================================
// EddyqRedis — Redis Functions–backed NAPI class
// ============================================================================
//
// A parallel surface to `Eddyq` for users on Redis. Skips Postgres-only ops
// (migrate, enqueueInTx). Reuses the JS-side type shapes (`EnqueueOptions`,
// `EnqueueOutcome`, `JobCall`, `ShutdownOptions`, …) so wrappers can route
// per-queue without users learning a second API.

type RedisCoreQueue = eddyq_core::Queue<RedisBackend>;
type RedisCoreQueueBuilder = eddyq_core::QueueBuilder<RedisBackend>;

/// Connection options for `EddyqRedis.connect`. Only the line/hash-tag
/// namespace is configurable in PR2; connection pooling is internal to the
/// `redis` crate's `ConnectionManager`.
#[napi(object)]
pub struct RedisConnectOptions {
    /// Hash-tag namespace (`"line"`) that scopes every key. Default `"main"`.
    /// Use distinct lines to isolate multiple logical queues on one Redis.
    pub line: Option<String>,
}

#[napi(js_name = "EddyqRedis")]
pub struct RedisQueue {
    backend: Arc<RedisBackend>,
    line: String,
    state: Arc<Mutex<RedisWorkerState>>,
    abort_handler: Arc<Mutex<Option<JsAbortFn>>>,
}

enum RedisWorkerState {
    Building {
        handlers: Vec<(String, JsHandler)>,
        concurrency: Option<usize>,
        subscribe: Option<Vec<String>>,
    },
    Running {
        queue: Arc<RedisCoreQueue>,
    },
    Stopped,
}

impl Default for RedisWorkerState {
    fn default() -> Self {
        Self::Building {
            handlers: Vec::new(),
            concurrency: None,
            subscribe: None,
        }
    }
}

fn rerr(e: eddyq_core::Error) -> napi::Error {
    napi::Error::from_reason(e.to_string())
}

#[napi]
impl RedisQueue {
    /// Connect to Redis and bootstrap-load the `eddyq_v1` Functions library.
    /// Safe to call concurrently from multiple replicas — library load is
    /// idempotent (compares SHA, replaces only on mismatch).
    #[napi(factory)]
    pub async fn connect(url: String, options: Option<RedisConnectOptions>) -> Result<RedisQueue> {
        let line = options
            .and_then(|o| o.line)
            .unwrap_or_else(|| "main".to_string());
        let cfg = RedisConfig {
            url,
            line: line.clone(),
        };
        let backend = RedisBackend::connect(cfg)
            .await
            .map_err(|e| napi::Error::from_reason(format!("{e}")))?;
        Ok(RedisQueue {
            backend: Arc::new(backend),
            line,
            state: Arc::new(Mutex::new(RedisWorkerState::default())),
            abort_handler: Arc::new(Mutex::new(None)),
        })
    }

    /// Hash-tag namespace ("line") this queue uses for all keys.
    #[napi(getter)]
    pub fn line(&self) -> &str {
        &self.line
    }

    /// Enqueue a single job.
    #[napi]
    pub async fn enqueue(
        &self,
        kind: String,
        payload: serde_json::Value,
        options: Option<EnqueueOptions>,
    ) -> Result<EnqueueOutcome> {
        let req = build_dyn_enqueue_from_opts(kind, payload, options)?;
        let result = self.backend.enqueue(req).await.map_err(rerr)?;
        Ok(match result {
            eddyq_core::EnqueueResult::Inserted(id) => EnqueueOutcome {
                inserted: true,
                id: Some(id),
            },
            eddyq_core::EnqueueResult::Skipped => EnqueueOutcome {
                inserted: false,
                id: None,
            },
        })
    }

    /// Bulk-enqueue jobs in a single round-trip. Mixed kinds supported.
    /// Mirrors the Postgres `enqueueMany` cap (5,000) so client code is
    /// portable across backends.
    #[napi]
    pub async fn enqueue_many(&self, items: Vec<EnqueueManyItem>) -> Result<BulkEnqueueOutcome> {
        if items.len() > 5_000 {
            return Err(napi::Error::from_reason(format!(
                "enqueueMany: batch of {} exceeds max of 5000; split client-side",
                items.len()
            )));
        }
        let mut reqs = Vec::with_capacity(items.len());
        for it in items {
            reqs.push(build_dyn_enqueue_from_many_item(it)?);
        }
        let res = self.backend.enqueue_many(reqs).await.map_err(rerr)?;
        Ok(BulkEnqueueOutcome {
            inserted: i64::try_from(res.inserted).unwrap_or(i64::MAX),
            skipped: i64::try_from(res.skipped).unwrap_or(i64::MAX),
        })
    }

    /// Cancel a pending or scheduled job. Returns `true` if cancelled,
    /// `false` if the job doesn't exist or is already running/finalized.
    #[napi]
    pub async fn cancel(&self, id: i64) -> Result<bool> {
        self.backend.cancel(id).await.map_err(rerr)
    }

    // --- Worker registration --------------------------------------------

    /// Register a JS handler for `kind`. Must be called before `start()`.
    #[napi(ts_args_type = "kind: string, handler: (call: JobCall) => Promise<unknown>")]
    pub fn work(&self, kind: String, handler: JsTsFn) -> Result<()> {
        self.register_handler_arc(kind, Arc::new(handler))
    }

    /// Rust-only path mirroring `Queue::register_handler_arc`. Takes an
    /// already-`Arc`'d handler so `EddyqApp` can fan one tsfn out to both
    /// backends without needing `ThreadsafeFunction: Clone`.
    pub(crate) fn register_handler_arc(&self, kind: String, handler: JsHandler) -> Result<()> {
        let mut state = self.state.lock().expect("redis worker state lock");
        match &mut *state {
            RedisWorkerState::Building { handlers, .. } => {
                handlers.push((kind, handler));
                Ok(())
            }
            RedisWorkerState::Running { .. } => Err(napi::Error::from_reason(
                "EddyqRedis is already running — register handlers before start()",
            )),
            RedisWorkerState::Stopped => {
                Err(napi::Error::from_reason("EddyqRedis has been shut down"))
            }
        }
    }

    /// Set worker concurrency (per-process). Default 10.
    #[napi]
    pub fn set_worker_concurrency(&self, n: u32) -> Result<()> {
        let mut state = self.state.lock().expect("redis worker state lock");
        match &mut *state {
            RedisWorkerState::Building { concurrency, .. } => {
                *concurrency = Some(n.max(1) as usize);
                Ok(())
            }
            _ => Err(napi::Error::from_reason(
                "setWorkerConcurrency must be called before start()",
            )),
        }
    }

    /// Subscribe workers to specific named queues. Default `["default"]`.
    #[napi]
    pub fn subscribe_to(&self, queues: Vec<String>) -> Result<()> {
        let mut state = self.state.lock().expect("redis worker state lock");
        match &mut *state {
            RedisWorkerState::Building { subscribe, .. } => {
                *subscribe = Some(queues);
                Ok(())
            }
            _ => Err(napi::Error::from_reason(
                "subscribeTo must be called before start()",
            )),
        }
    }

    /// Start the worker runtime. Errors if no handlers were registered, or if
    /// already running.
    #[napi]
    pub async fn start(&self, options: Option<StartOptions>) -> Result<()> {
        let (handlers, concurrency, subscribe) = {
            let mut state = self.state.lock().expect("redis worker state lock");
            match std::mem::replace(&mut *state, RedisWorkerState::Stopped) {
                RedisWorkerState::Building {
                    handlers,
                    concurrency,
                    subscribe,
                } => (handlers, concurrency, subscribe),
                RedisWorkerState::Running { queue } => {
                    *state = RedisWorkerState::Running { queue };
                    return Err(napi::Error::from_reason("EddyqRedis is already running"));
                }
                RedisWorkerState::Stopped => {
                    return Err(napi::Error::from_reason(
                        "EddyqRedis has been shut down and cannot be reused",
                    ));
                }
            }
        };
        if handlers.is_empty() {
            let mut state = self.state.lock().expect("redis worker state lock");
            *state = RedisWorkerState::Building {
                handlers: Vec::new(),
                concurrency,
                subscribe,
            };
            return Err(napi::Error::from_reason(
                "No handlers registered — call work(kind, fn) before start()",
            ));
        }

        // Build the typed Queue<RedisBackend>. Reuses the same StartOptions
        // shape as PG; not every knob applies (e.g. fetch_poll_interval is
        // mostly a poll-fallback knob since Redis has pubsub disabled in PR2).
        let backend = (*self.backend).clone();
        let mut builder: RedisCoreQueueBuilder =
            RedisCoreQueueBuilder::with_backend(backend).line(self.line.clone());
        if let Some(n) = concurrency {
            builder = builder.worker_concurrency(n);
        }
        if let Some(qs) = subscribe {
            builder = builder.subscribe_to(qs);
        }
        if let Some(o) = options.as_ref() {
            if let Some(ms) = o.sweep_interval_ms {
                builder = builder.sweep_interval(Duration::from_millis(u64::from(ms)));
            }
            if let Some(ms) = o.stale_after_ms {
                builder = builder.stale_after(Duration::from_millis(u64::from(ms)));
            }
            if let Some(ms) = o.heartbeat_interval_ms {
                builder = builder.heartbeat_interval(Duration::from_millis(u64::from(ms)));
            }
            if let Some(ms) = o.cleanup_interval_ms {
                builder = builder.cleanup_interval(Duration::from_millis(u64::from(ms)));
            }
            if let Some(secs) = o.leader_lease_secs {
                builder = builder.leader_lease_secs(u64::from(secs));
            }
            if let Some(ms) = o.fetch_poll_interval_ms {
                builder = builder.fetch_poll_interval(Duration::from_millis(u64::from(ms)));
            }
            if let Some(ms) = o.scheduler_interval_ms {
                builder = builder.scheduler_interval(Duration::from_millis(u64::from(ms)));
            }
        }
        for (kind, tsfn) in handlers {
            builder = builder.register_dyn(kind, dispatcher(tsfn));
        }

        let queue = Arc::new(builder.build());
        queue
            .start()
            .map_err(|e| napi::Error::from_reason(e.to_string()))?;

        let mut state = self.state.lock().expect("redis worker state lock");
        *state = RedisWorkerState::Running { queue };
        Ok(())
    }

    /// Set the abort-broadcast handler. Called once per shutdown so the JS
    /// wrapper can fire `.abort()` on all in-flight `AbortController`s.
    #[napi(ts_args_type = "handler: (reason: string) => void")]
    pub fn set_abort_handler(&self, handler: JsAbortFn) -> Result<()> {
        let mut slot = self.abort_handler.lock().expect("abort handler lock");
        *slot = Some(handler);
        Ok(())
    }

    /// Graceful shutdown: stop claiming new jobs, fire `AbortSignal` to
    /// in-flight handlers, then await them. Modes mirror the Postgres
    /// `Eddyq.shutdown`.
    #[napi]
    pub async fn shutdown(&self, options: Option<ShutdownOptions>) -> Result<()> {
        let queue = {
            let mut state = self.state.lock().expect("redis worker state lock");
            match std::mem::replace(&mut *state, RedisWorkerState::Stopped) {
                RedisWorkerState::Running { queue } => queue,
                _ => return Err(napi::Error::from_reason("EddyqRedis is not running")),
            }
        };
        let mode = options
            .as_ref()
            .and_then(|o| o.mode.as_deref())
            .map(parse_shutdown_mode)
            .transpose()?
            .unwrap_or(ShutdownMode::Drain);
        if let Some(handler) = self.abort_handler.lock().expect("abort lock").as_ref() {
            let reason = match mode {
                ShutdownMode::Drain => "drain",
                ShutdownMode::Force => "force",
                ShutdownMode::Abandon => "abandon",
            };
            handler.call(reason.to_string(), ThreadsafeFunctionCallMode::NonBlocking);
        }
        queue
            .shutdown_with(mode)
            .await
            .map_err(|e| napi::Error::from_reason(e.to_string()))
    }

    // --- Group admin -----------------------------------------------------

    #[napi]
    pub async fn set_group_concurrency(&self, group_key: String, max: i32) -> Result<()> {
        self.backend
            .group_set_concurrency(&group_key, max)
            .await
            .map_err(rerr)
    }
    #[napi]
    pub async fn pause_group(&self, group_key: String) -> Result<()> {
        self.backend
            .group_set_paused(&group_key, true)
            .await
            .map_err(rerr)
    }
    #[napi]
    pub async fn resume_group(&self, group_key: String) -> Result<()> {
        self.backend
            .group_set_paused(&group_key, false)
            .await
            .map_err(rerr)
    }
    #[napi]
    pub async fn set_group_rate(
        &self,
        group_key: String,
        count: u32,
        period_ms: u32,
    ) -> Result<()> {
        self.backend
            .group_set_rate(
                &group_key,
                count,
                Duration::from_millis(u64::from(period_ms)),
            )
            .await
            .map_err(rerr)
    }
    #[napi]
    pub async fn clear_group_rate(&self, group_key: String) -> Result<()> {
        self.backend
            .group_clear_rate(&group_key)
            .await
            .map_err(rerr)
    }
    #[napi]
    pub async fn list_groups(&self) -> Result<Vec<Group>> {
        let groups = self.backend.group_list().await.map_err(rerr)?;
        Ok(groups.into_iter().map(group_to_napi).collect())
    }
    #[napi]
    pub async fn get_group(&self, key: String) -> Result<Option<Group>> {
        Ok(self
            .backend
            .group_get(&key)
            .await
            .map_err(rerr)?
            .map(group_to_napi))
    }

    // --- Named-queue admin ----------------------------------------------

    #[napi]
    pub async fn set_queue_concurrency(&self, queue: String, max: i32) -> Result<()> {
        self.backend
            .queue_set_concurrency(&queue, max)
            .await
            .map_err(rerr)
    }
    #[napi]
    pub async fn pause_queue(&self, queue: String) -> Result<()> {
        self.backend
            .queue_set_paused(&queue, true)
            .await
            .map_err(rerr)
    }
    #[napi]
    pub async fn resume_queue(&self, queue: String) -> Result<()> {
        self.backend
            .queue_set_paused(&queue, false)
            .await
            .map_err(rerr)
    }
    #[napi]
    pub async fn set_queue_timeout(&self, queue: String, timeout_ms: Option<u32>) -> Result<()> {
        let t = timeout_ms.map(|ms| Duration::from_millis(u64::from(ms)));
        self.backend
            .queue_set_timeout(&queue, t)
            .await
            .map_err(rerr)
    }
    #[napi]
    pub async fn list_named_queues(&self) -> Result<Vec<NamedQueue>> {
        let qs = self.backend.queue_list().await.map_err(rerr)?;
        Ok(qs.into_iter().map(nq_to_napi).collect())
    }
    #[napi]
    pub async fn get_queue(&self, name: String) -> Result<Option<NamedQueue>> {
        Ok(self
            .backend
            .queue_get(&name)
            .await
            .map_err(rerr)?
            .map(nq_to_napi))
    }

    // --- Schedules ------------------------------------------------------

    #[napi]
    #[allow(clippy::too_many_arguments)]
    pub async fn add_schedule(
        &self,
        name: String,
        cron: String,
        kind: String,
        payload: serde_json::Value,
        priority: Option<i32>,
        max_attempts: Option<i32>,
        queue: Option<String>,
    ) -> Result<()> {
        let prio = i16::try_from(priority.unwrap_or(0)).unwrap_or(0);
        let max_att = max_attempts.unwrap_or(3);
        let q = queue.unwrap_or_else(|| eddyq_core::DEFAULT_QUEUE.to_owned());
        self.backend
            .upsert_schedule_raw(&name, &cron, &kind, payload, prio, max_att, &q)
            .await
            .map_err(rerr)
    }
    /// Register a fixed-interval schedule. Fires every `intervalMs`
    /// milliseconds — no cron expression required. Mirrors BullMQ's
    /// `upsertJobScheduler(id, { every })`.
    ///
    /// `intervalMs` must be positive. The first fire happens after
    /// `intervalMs` from registration; subsequent fires are
    /// `previous_fire + intervalMs` (no catch-up under leader downtime —
    /// missed ticks are skipped, matching the cron path's semantics).
    #[napi]
    #[allow(clippy::too_many_arguments)]
    pub async fn add_interval_schedule(
        &self,
        name: String,
        interval_ms: i64,
        kind: String,
        payload: serde_json::Value,
        priority: Option<i32>,
        max_attempts: Option<i32>,
        queue: Option<String>,
    ) -> Result<()> {
        let prio = i16::try_from(priority.unwrap_or(0)).unwrap_or(0);
        let max_att = max_attempts.unwrap_or(3);
        let q = queue.unwrap_or_else(|| eddyq_core::DEFAULT_QUEUE.to_owned());
        self.backend
            .upsert_interval_schedule_raw(&name, interval_ms, &kind, payload, prio, max_att, &q)
            .await
            .map_err(rerr)
    }

    #[napi]
    pub async fn remove_schedule(&self, name: String) -> Result<bool> {
        self.backend.remove_schedule(&name).await.map_err(rerr)
    }
    #[napi]
    pub async fn set_schedule_enabled(&self, name: String, enabled: bool) -> Result<bool> {
        self.backend
            .set_schedule_enabled(&name, enabled)
            .await
            .map_err(rerr)
    }
    #[napi]
    pub async fn list_schedules(&self) -> Result<Vec<Schedule>> {
        let list = self.backend.list_schedules().await.map_err(rerr)?;
        Ok(list.into_iter().map(schedule_to_napi).collect())
    }

    // --- Stats / list_jobs --------------------------------------------

    /// Snapshot of job counts grouped by (queue, state). Single Redis
    /// round-trip — suitable as the landing query for a dashboard.
    #[napi]
    pub async fn get_stats(&self) -> Result<JobStats> {
        let s = self.backend.get_stats().await.map_err(rerr)?;
        Ok(JobStats {
            by_queue_state: s
                .by_queue_state
                .into_iter()
                .map(|c| QueueStateCount {
                    queue: c.queue,
                    state: state_to_str(c.state).to_string(),
                    count: c.count,
                })
                .collect(),
        })
    }

    /// Paginated job listing. Defaults: limit=50, offset=0 (capped at 500).
    #[napi]
    pub async fn list_jobs(
        &self,
        filter: Option<ListJobsFilter>,
        pagination: Option<Pagination>,
    ) -> Result<JobList> {
        let core_filter = filter.map(napi_filter_to_core).unwrap_or_default();
        let core_pagination = pagination
            .map(|p| eddyq_core::stats::Pagination {
                limit: p.limit.unwrap_or(50),
                offset: p.offset.unwrap_or(0),
            })
            .unwrap_or_default();
        let list = self
            .backend
            .list_jobs(core_filter, core_pagination)
            .await
            .map_err(rerr)?;
        Ok(JobList {
            total: list.total,
            rows: list.rows.into_iter().map(job_row_to_napi).collect(),
        })
    }

    /// Reconcile schedules against a declared list. Same semantics as the
    /// Postgres `Eddyq.syncSchedules` — upserts every declared entry and
    /// deletes any stored schedule not in the list. Idempotent.
    #[napi]
    pub async fn sync_schedules(
        &self,
        declared: Vec<ScheduleDeclaration>,
    ) -> Result<SyncSchedulesReport> {
        let mapped: Vec<CoreScheduleDeclaration> = declared
            .into_iter()
            .map(|d| CoreScheduleDeclaration {
                name: d.name,
                cron_expr: d.cron_expr,
                kind: d.kind,
                payload: d.payload,
                priority: d.priority.unwrap_or(0),
                max_attempts: d.max_attempts.unwrap_or(3),
                queue: d
                    .queue
                    .unwrap_or_else(|| eddyq_core::DEFAULT_QUEUE.to_string()),
            })
            .collect();
        let report = self.backend.sync_schedules(&mapped).await.map_err(rerr)?;
        Ok(SyncSchedulesReport {
            upserted: report.upserted as u32,
            deleted: report.deleted,
        })
    }
}

// Shared helpers — build DynEnqueue from the JS-side option shapes. Pulled
// out so both Postgres and Redis paths reuse the same field-by-field mapping.
fn build_dyn_enqueue_from_opts(
    kind: String,
    payload: serde_json::Value,
    options: Option<EnqueueOptions>,
) -> Result<DynEnqueue> {
    let mut req = DynEnqueue::new(kind, payload);
    if let Some(opts) = options {
        if let Some(n) = opts.max_attempts {
            req.max_attempts = n;
        }
        if let Some(p) = opts.priority {
            req.priority = p;
        }
        if let Some(q) = opts.queue {
            req.queue = q;
        }
        if opts.scheduled_at_ms.is_some() && opts.delay_ms.is_some() {
            return Err(napi::Error::from_reason(
                "enqueue: pass either scheduledAtMs or delayMs, not both",
            ));
        }
        if let Some(ms) = opts.scheduled_at_ms {
            req.scheduled_at = Some(ms_to_utc(ms));
        }
        if let Some(ms) = opts.delay_ms {
            req.scheduled_at = Some(Utc::now() + chrono::Duration::milliseconds(ms));
        }
        if let Some(k) = opts.unique_key {
            req.unique_key = Some(k);
        }
        if let Some(g) = opts.group_key {
            req.group_key = Some(g);
        }
        if let Some(t) = opts.tags {
            req.tags = t;
        }
        if let Some(m) = opts.metadata {
            req.metadata = m;
        }
    }
    Ok(req)
}

fn build_dyn_enqueue_from_many_item(it: EnqueueManyItem) -> Result<DynEnqueue> {
    let mut req = DynEnqueue::new(it.kind, it.payload);
    if let Some(n) = it.max_attempts {
        req.max_attempts = n;
    }
    if let Some(p) = it.priority {
        req.priority = p;
    }
    if let Some(q) = it.queue {
        req.queue = q;
    }
    if it.scheduled_at_ms.is_some() && it.delay_ms.is_some() {
        return Err(napi::Error::from_reason(
            "enqueueMany: pass either scheduledAtMs or delayMs, not both",
        ));
    }
    if let Some(ms) = it.scheduled_at_ms {
        req.scheduled_at = Some(ms_to_utc(ms));
    }
    if let Some(ms) = it.delay_ms {
        req.scheduled_at = Some(Utc::now() + chrono::Duration::milliseconds(ms));
    }
    if let Some(k) = it.unique_key {
        req.unique_key = Some(k);
    }
    if let Some(g) = it.group_key {
        req.group_key = Some(g);
    }
    if let Some(t) = it.tags {
        req.tags = t;
    }
    if let Some(m) = it.metadata {
        req.metadata = m;
    }
    Ok(req)
}

fn parse_shutdown_mode(s: &str) -> Result<ShutdownMode> {
    match s {
        "drain" => Ok(ShutdownMode::Drain),
        "force" => Ok(ShutdownMode::Force),
        "abandon" => Ok(ShutdownMode::Abandon),
        other => Err(napi::Error::from_reason(format!(
            "shutdown: invalid mode {other:?} (drain | force | abandon)"
        ))),
    }
}

// ============================================================================
// EddyqApp — single-process multi-backend container.
//
// Holds an optional `Eddyq` + an optional `EddyqRedis` and routes `enqueue` /
// worker pickup per queue. Use when one Nest (or plain Node) app wants e.g.
// `webhooks` on Redis and `payments` on Postgres.
// ============================================================================

/// One queue→backend binding for `EddyqAppConfig.queues`.
#[napi(object)]
pub struct EddyqAppQueueRoute {
    /// Queue name (must match what callers pass as `enqueue(..., { queue })`).
    pub name: String,
    /// `"postgres"` or `"redis"`. Validated at construction.
    pub provider: String,
}

#[napi(object)]
pub struct EddyqAppPgConfig {
    pub database_url: String,
    pub options: Option<ConnectOptions>,
}

#[napi(object)]
pub struct EddyqAppRedisConfig {
    pub url: String,
    pub line: Option<String>,
}

/// Top-level config for `EddyqApp.connect`. Both `postgres` and `redis` are
/// optional, but at least one must be set. Queues whose `name` isn't in
/// `queues` route to `defaultProvider` (which defaults to whichever backend
/// you configured if only one is set).
#[napi(object)]
pub struct EddyqAppConfig {
    pub postgres: Option<EddyqAppPgConfig>,
    pub redis: Option<EddyqAppRedisConfig>,
    pub queues: Vec<EddyqAppQueueRoute>,
    pub default_provider: Option<String>,
}

#[derive(Clone, Copy, PartialEq)]
enum BackendKind {
    Pg,
    Redis,
}

fn parse_provider(s: &str) -> Result<BackendKind> {
    match s {
        "postgres" | "pg" => Ok(BackendKind::Pg),
        "redis" => Ok(BackendKind::Redis),
        other => Err(napi::Error::from_reason(format!(
            "unknown provider {other:?} — expected 'postgres' or 'redis'"
        ))),
    }
}

#[napi(js_name = "EddyqApp")]
pub struct EddyqApp {
    pg: Option<Arc<Queue>>,
    redis: Option<Arc<RedisQueue>>,
    routes: std::collections::HashMap<String, BackendKind>,
    default_provider: BackendKind,
    abort_handler: Arc<Mutex<Option<JsAbortFn>>>,
}

#[napi]
impl EddyqApp {
    /// Construct + connect both backends. `queues` declares the routing
    /// table (queue name → "postgres" | "redis"). Unrouted queue names fall
    /// back to `defaultProvider`; if that's omitted, the only configured
    /// backend wins.
    #[napi(factory)]
    pub async fn connect(config: EddyqAppConfig) -> Result<EddyqApp> {
        let want_pg = config.postgres.is_some();
        let want_redis = config.redis.is_some();
        if !want_pg && !want_redis {
            return Err(napi::Error::from_reason(
                "EddyqApp.connect: at least one of `postgres` or `redis` must be set",
            ));
        }

        // Build the routing map up front so we can fail-fast on typos.
        let mut routes: std::collections::HashMap<String, BackendKind> =
            std::collections::HashMap::new();
        for r in &config.queues {
            let kind = parse_provider(&r.provider)?;
            if matches!(kind, BackendKind::Pg) && !want_pg {
                return Err(napi::Error::from_reason(format!(
                    "queue {:?} routes to 'postgres' but no postgres config provided",
                    r.name
                )));
            }
            if matches!(kind, BackendKind::Redis) && !want_redis {
                return Err(napi::Error::from_reason(format!(
                    "queue {:?} routes to 'redis' but no redis config provided",
                    r.name
                )));
            }
            routes.insert(r.name.clone(), kind);
        }

        let default_provider = match config.default_provider.as_deref() {
            Some(s) => parse_provider(s)?,
            None => {
                // Single-backend setup picks itself as default.
                if want_pg && !want_redis {
                    BackendKind::Pg
                } else if want_redis && !want_pg {
                    BackendKind::Redis
                } else {
                    return Err(napi::Error::from_reason(
                        "EddyqApp.connect: both backends configured — set `defaultProvider`",
                    ));
                }
            }
        };

        let pg = if let Some(c) = config.postgres {
            Some(Arc::new(Queue::connect(c.database_url, c.options).await?))
        } else {
            None
        };
        let redis = if let Some(c) = config.redis {
            let opts = c.line.map(|line| RedisConnectOptions { line: Some(line) });
            Some(Arc::new(RedisQueue::connect(c.url, opts).await?))
        } else {
            None
        };

        // Pre-subscribe each backend to the queues that route to it. The user
        // can override later via the app-level subscribeTo helpers if they
        // want a worker-only process that subscribes to a subset.
        let mut pg_queues: Vec<String> = Vec::new();
        let mut redis_queues: Vec<String> = Vec::new();
        for r in &config.queues {
            match parse_provider(&r.provider)? {
                BackendKind::Pg => pg_queues.push(r.name.clone()),
                BackendKind::Redis => redis_queues.push(r.name.clone()),
            }
        }
        if let (Some(p), false) = (&pg, pg_queues.is_empty()) {
            p.subscribe_to(pg_queues)?;
        }
        if let (Some(r), false) = (&redis, redis_queues.is_empty()) {
            r.subscribe_to(redis_queues)?;
        }

        Ok(EddyqApp {
            pg,
            redis,
            routes,
            default_provider,
            abort_handler: Arc::new(Mutex::new(None)),
        })
    }

    fn pick(&self, queue_name: &str) -> BackendKind {
        self.routes
            .get(queue_name)
            .copied()
            .unwrap_or(self.default_provider)
    }

    fn pg_ref(&self) -> Result<&Arc<Queue>> {
        self.pg
            .as_ref()
            .ok_or_else(|| napi::Error::from_reason("postgres backend not configured"))
    }

    fn redis_ref(&self) -> Result<&Arc<RedisQueue>> {
        self.redis
            .as_ref()
            .ok_or_else(|| napi::Error::from_reason("redis backend not configured"))
    }

    /// Enqueue a job. The target backend is picked from `options.queue` via
    /// the route table — falling back to `defaultProvider` when the queue
    /// isn't in the table.
    #[napi]
    pub async fn enqueue(
        &self,
        kind: String,
        payload: serde_json::Value,
        options: Option<EnqueueOptions>,
    ) -> Result<EnqueueOutcome> {
        let queue_name = options
            .as_ref()
            .and_then(|o| o.queue.clone())
            .unwrap_or_else(|| "default".to_string());
        match self.pick(&queue_name) {
            BackendKind::Pg => {
                let q = self
                    .pg
                    .as_ref()
                    .ok_or_else(|| napi::Error::from_reason("postgres backend not configured"))?;
                q.enqueue(kind, payload, options).await
            }
            BackendKind::Redis => {
                let q = self
                    .redis
                    .as_ref()
                    .ok_or_else(|| napi::Error::from_reason("redis backend not configured"))?;
                q.enqueue(kind, payload, options).await
            }
        }
    }

    /// Per-backend job-state snapshot. Same shape as the standalone
    /// `Eddyq.getStats` / `EddyqRedis.getStats` — just scoped to one
    /// backend so the dashboard can render each half independently.
    #[napi]
    pub async fn get_stats_for(&self, provider: String) -> Result<JobStats> {
        match parse_provider(&provider)? {
            BackendKind::Pg => self.pg_ref()?.get_stats().await,
            BackendKind::Redis => self.redis_ref()?.get_stats().await,
        }
    }

    /// Which backend a queue routes to. Returns `"postgres"` or `"redis"`
    /// based on the routing table + `defaultProvider`. Useful when callers
    /// need to pick provider-specific paths (e.g. NestJS `@InjectQueue`
    /// handles dispatching admin calls correctly).
    #[napi]
    pub fn provider_for(&self, queue_name: String) -> String {
        match self.pick(&queue_name) {
            BackendKind::Pg => "postgres".to_string(),
            BackendKind::Redis => "redis".to_string(),
        }
    }

    /// Whether this app has a Postgres backend configured. Lets callers
    /// decide whether to call PG-only methods (`migrate`, `enqueueBatch`).
    #[napi(getter)]
    pub fn has_postgres(&self) -> bool {
        self.pg.is_some()
    }

    /// Whether this app has a Redis backend configured.
    #[napi(getter)]
    pub fn has_redis(&self) -> bool {
        self.redis.is_some()
    }

    /// Apply pending Postgres migrations (no-op when no PG backend).
    /// Mirrors `Eddyq.migrate` so the Nest module can call it uniformly.
    #[napi]
    pub async fn migrate(&self) -> Result<Option<MigrateReport>> {
        match &self.pg {
            Some(p) => Ok(Some(p.migrate().await?)),
            None => Ok(None),
        }
    }

    /// Migration status for the Postgres backend (no-op when no PG backend).
    #[napi]
    pub async fn migration_status(&self) -> Result<Vec<MigrationStatus>> {
        match &self.pg {
            Some(p) => p.migration_status().await,
            None => Ok(Vec::new()),
        }
    }

    /// Close the Postgres pool (no-op when no PG backend).
    #[napi]
    pub async fn close(&self) -> Result<()> {
        if let Some(p) = &self.pg {
            p.close().await?;
        }
        Ok(())
    }

    /// Subscribe each backend's worker pool to the queues that route to it.
    /// The first time this is called the routes from `connect()` take
    /// effect; subsequent calls let runtime callers (e.g. NestJS dynamic
    /// queue registration) extend the subscribed set.
    #[napi]
    pub fn subscribe_to(&self, queues: Vec<String>) -> Result<()> {
        let mut pg_q: Vec<String> = Vec::new();
        let mut redis_q: Vec<String> = Vec::new();
        for q in queues {
            match self.pick(&q) {
                BackendKind::Pg => pg_q.push(q),
                BackendKind::Redis => redis_q.push(q),
            }
        }
        if let (Some(p), false) = (&self.pg, pg_q.is_empty()) {
            p.subscribe_to(pg_q)?;
        }
        if let (Some(r), false) = (&self.redis, redis_q.is_empty()) {
            r.subscribe_to(redis_q)?;
        }
        Ok(())
    }

    /// Set worker concurrency on both backends.
    #[napi]
    pub fn set_worker_concurrency(&self, n: u32) -> Result<()> {
        if let Some(p) = &self.pg {
            p.set_worker_concurrency(n)?;
        }
        if let Some(r) = &self.redis {
            r.set_worker_concurrency(n)?;
        }
        Ok(())
    }

    /// Reconcile the schedule list against one provider's backend. The
    /// declared entries are upserted, anything not in the list is deleted.
    /// Idempotent — safe to run on every boot. Useful when schedules are
    /// declared in module config and the caller wants to keep them in sync.
    #[napi]
    pub async fn sync_schedules(
        &self,
        provider: String,
        declared: Vec<ScheduleDeclaration>,
    ) -> Result<SyncSchedulesReport> {
        match parse_provider(&provider)? {
            BackendKind::Pg => self.pg_ref()?.sync_schedules(declared).await,
            BackendKind::Redis => self.redis_ref()?.sync_schedules(declared).await,
        }
    }

    /// Bulk-enqueue with per-item routing. Items are grouped by their
    /// resolved provider (per `options.queue` → routes → `defaultProvider`),
    /// then dispatched to each backend in a single `enqueueMany` call.
    /// The returned counts are the sum across both backends.
    ///
    /// Mixed batches across backends are supported in one call — useful for
    /// fan-in jobs where some children land on Redis (e.g. webhook delivery)
    /// and some on Postgres (e.g. ledger write).
    #[napi]
    pub async fn enqueue_many(&self, items: Vec<EnqueueManyItem>) -> Result<BulkEnqueueOutcome> {
        let mut pg_items: Vec<EnqueueManyItem> = Vec::new();
        let mut redis_items: Vec<EnqueueManyItem> = Vec::new();
        for item in items {
            let q = item.queue.clone().unwrap_or_else(|| "default".to_owned());
            match self.pick(&q) {
                BackendKind::Pg => pg_items.push(item),
                BackendKind::Redis => redis_items.push(item),
            }
        }
        let mut total_inserted: i64 = 0;
        let mut total_skipped: i64 = 0;
        if !pg_items.is_empty() {
            let q = self
                .pg
                .as_ref()
                .ok_or_else(|| napi::Error::from_reason("postgres backend not configured"))?;
            let r = q.enqueue_many(pg_items).await?;
            total_inserted += r.inserted;
            total_skipped += r.skipped;
        }
        if !redis_items.is_empty() {
            let q = self
                .redis
                .as_ref()
                .ok_or_else(|| napi::Error::from_reason("redis backend not configured"))?;
            let r = q.enqueue_many(redis_items).await?;
            total_inserted += r.inserted;
            total_skipped += r.skipped;
        }
        Ok(BulkEnqueueOutcome {
            inserted: total_inserted,
            skipped: total_skipped,
        })
    }

    /// Cancel a pending job. The caller must specify which provider owns the
    /// id — job ids are not globally unique across backends.
    #[napi]
    pub async fn cancel(&self, id: i64, provider: String) -> Result<bool> {
        match parse_provider(&provider)? {
            BackendKind::Pg => {
                self.pg
                    .as_ref()
                    .ok_or_else(|| napi::Error::from_reason("postgres backend not configured"))?
                    .cancel(id)
                    .await
            }
            BackendKind::Redis => {
                self.redis
                    .as_ref()
                    .ok_or_else(|| napi::Error::from_reason("redis backend not configured"))?
                    .cancel(id)
                    .await
            }
        }
    }

    /// Register a handler for `kind`. The same handler is registered on
    /// every configured backend — only the backend that actually fetches a
    /// job of this kind (per its `subscribeTo`) will invoke it.
    #[napi(ts_args_type = "kind: string, handler: (call: JobCall) => Promise<unknown>")]
    pub fn work(&self, kind: String, handler: JsTsFn) -> Result<()> {
        // `ThreadsafeFunction` itself isn't Clone, but the `JsHandler` newtype
        // (= `Arc<JsTsFn>`) is. Wrap once, share between backends — each
        // backend stores its own Arc handle and the JS function survives
        // until both backends are dropped.
        let shared: JsHandler = Arc::new(handler);
        if let Some(p) = &self.pg {
            p.register_handler_arc(kind.clone(), shared.clone())?;
        }
        if let Some(r) = &self.redis {
            r.register_handler_arc(kind, shared)?;
        }
        Ok(())
    }

    // --- Admin: groups -----------------------------------------------------
    //
    // Admin calls take a `provider` arg ("postgres" | "redis") because groups
    // are namespaced per backend — a `tenant-acme` group on Redis is a
    // different bucket than the same name on Postgres. Callers wanting
    // cross-backend admin can call the method twice.

    #[napi]
    pub async fn set_group_concurrency(
        &self,
        provider: String,
        group_key: String,
        max: i32,
    ) -> Result<()> {
        match parse_provider(&provider)? {
            BackendKind::Pg => self.pg_ref()?.set_group_concurrency(group_key, max).await,
            BackendKind::Redis => {
                self.redis_ref()?
                    .set_group_concurrency(group_key, max)
                    .await
            }
        }
    }

    #[napi]
    pub async fn pause_group(&self, provider: String, group_key: String) -> Result<()> {
        match parse_provider(&provider)? {
            BackendKind::Pg => self.pg_ref()?.pause_group(group_key).await,
            BackendKind::Redis => self.redis_ref()?.pause_group(group_key).await,
        }
    }

    #[napi]
    pub async fn resume_group(&self, provider: String, group_key: String) -> Result<()> {
        match parse_provider(&provider)? {
            BackendKind::Pg => self.pg_ref()?.resume_group(group_key).await,
            BackendKind::Redis => self.redis_ref()?.resume_group(group_key).await,
        }
    }

    #[napi]
    pub async fn set_group_rate(
        &self,
        provider: String,
        group_key: String,
        count: u32,
        period_ms: u32,
    ) -> Result<()> {
        match parse_provider(&provider)? {
            BackendKind::Pg => {
                self.pg_ref()?
                    .set_group_rate(group_key, count, period_ms)
                    .await
            }
            BackendKind::Redis => {
                self.redis_ref()?
                    .set_group_rate(group_key, count, period_ms)
                    .await
            }
        }
    }

    #[napi]
    pub async fn clear_group_rate(&self, provider: String, group_key: String) -> Result<()> {
        match parse_provider(&provider)? {
            BackendKind::Pg => self.pg_ref()?.clear_group_rate(group_key).await,
            BackendKind::Redis => self.redis_ref()?.clear_group_rate(group_key).await,
        }
    }

    #[napi]
    pub async fn list_groups(&self, provider: String) -> Result<Vec<Group>> {
        match parse_provider(&provider)? {
            BackendKind::Pg => self.pg_ref()?.list_groups().await,
            BackendKind::Redis => self.redis_ref()?.list_groups().await,
        }
    }

    /// Per-backend named-queue listing. Dashboards typically render one
    /// list per backend (since queue names aren't globally unique across
    /// backends — they can collide deliberately or accidentally).
    #[napi]
    pub async fn list_named_queues(&self, provider: String) -> Result<Vec<NamedQueue>> {
        match parse_provider(&provider)? {
            BackendKind::Pg => self.pg_ref()?.list_named_queues().await,
            BackendKind::Redis => self.redis_ref()?.list_named_queues().await,
        }
    }

    /// Paginated job listing. Routes by `filter.queue` if set, otherwise
    /// uses the default provider. Pagination doesn't compose across two
    /// backends — callers wanting cross-backend results should issue two
    /// separate calls and stitch UI-side.
    #[napi]
    pub async fn list_jobs(
        &self,
        filter: Option<ListJobsFilter>,
        pagination: Option<Pagination>,
    ) -> Result<JobList> {
        let queue = filter
            .as_ref()
            .and_then(|f| f.queue.clone())
            .unwrap_or_else(|| "default".to_owned());
        match self.pick(&queue) {
            BackendKind::Pg => self.pg_ref()?.list_jobs(filter, pagination).await,
            BackendKind::Redis => self.redis_ref()?.list_jobs(filter, pagination).await,
        }
    }

    // --- Admin: named queues ----------------------------------------------

    #[napi]
    pub async fn set_queue_concurrency(
        &self,
        provider: String,
        queue: String,
        max: i32,
    ) -> Result<()> {
        match parse_provider(&provider)? {
            BackendKind::Pg => self.pg_ref()?.set_queue_concurrency(queue, max).await,
            BackendKind::Redis => self.redis_ref()?.set_queue_concurrency(queue, max).await,
        }
    }

    #[napi]
    pub async fn pause_queue(&self, provider: String, queue: String) -> Result<()> {
        match parse_provider(&provider)? {
            BackendKind::Pg => self.pg_ref()?.pause_queue(queue).await,
            BackendKind::Redis => self.redis_ref()?.pause_queue(queue).await,
        }
    }

    #[napi]
    pub async fn resume_queue(&self, provider: String, queue: String) -> Result<()> {
        match parse_provider(&provider)? {
            BackendKind::Pg => self.pg_ref()?.resume_queue(queue).await,
            BackendKind::Redis => self.redis_ref()?.resume_queue(queue).await,
        }
    }

    // --- Admin: schedules --------------------------------------------------
    //
    // Schedules are also per-backend. The convention is that schedule names
    // are unique within a provider; callers using both backends should
    // namespace by hand (e.g. `pg:nightly-report` and `redis:cache-warm`).

    #[napi]
    #[allow(clippy::too_many_arguments)]
    pub async fn add_schedule(
        &self,
        provider: String,
        name: String,
        cron: String,
        kind: String,
        payload: serde_json::Value,
        priority: Option<i32>,
        max_attempts: Option<i32>,
        queue: Option<String>,
    ) -> Result<()> {
        match parse_provider(&provider)? {
            BackendKind::Pg => {
                let opts = ScheduleOptions {
                    priority: priority.map(|p| i16::try_from(p).unwrap_or(0)),
                    max_attempts,
                    queue,
                };
                self.pg_ref()?
                    .add_schedule(name, cron, kind, payload, Some(opts))
                    .await
            }
            BackendKind::Redis => {
                self.redis_ref()?
                    .add_schedule(name, cron, kind, payload, priority, max_attempts, queue)
                    .await
            }
        }
    }

    /// Interval-style schedule — Redis only (matches the underlying class).
    #[napi]
    #[allow(clippy::too_many_arguments)]
    pub async fn add_interval_schedule(
        &self,
        name: String,
        interval_ms: i64,
        kind: String,
        payload: serde_json::Value,
        priority: Option<i32>,
        max_attempts: Option<i32>,
        queue: Option<String>,
    ) -> Result<()> {
        self.redis_ref()?
            .add_interval_schedule(
                name,
                interval_ms,
                kind,
                payload,
                priority,
                max_attempts,
                queue,
            )
            .await
    }

    #[napi]
    pub async fn remove_schedule(&self, provider: String, name: String) -> Result<bool> {
        match parse_provider(&provider)? {
            BackendKind::Pg => self.pg_ref()?.remove_schedule(name).await,
            BackendKind::Redis => self.redis_ref()?.remove_schedule(name).await,
        }
    }

    #[napi]
    pub async fn list_schedules(&self, provider: String) -> Result<Vec<Schedule>> {
        match parse_provider(&provider)? {
            BackendKind::Pg => self.pg_ref()?.list_schedules().await,
            BackendKind::Redis => self.redis_ref()?.list_schedules().await,
        }
    }

    /// Register the abort-broadcast handler. lib.cjs uses this to fan out
    /// shutdown to in-flight `AbortController`s. Stored on the `EddyqApp`
    /// itself — fired once at the start of `shutdown()`, before either
    /// inner backend drains, so handlers see the signal regardless of which
    /// backend they're running on.
    #[napi(ts_args_type = "handler: (reason: string) => void")]
    pub fn set_abort_handler(&self, handler: JsAbortFn) -> Result<()> {
        let mut slot = self.abort_handler.lock().expect("abort lock");
        *slot = Some(handler);
        Ok(())
    }

    /// Start both backends' worker runtimes. Tuning options apply to both.
    #[napi]
    pub async fn start(&self, options: Option<StartOptions>) -> Result<()> {
        if let Some(p) = &self.pg {
            p.start(options.as_ref().map(clone_start_options)).await?;
        }
        if let Some(r) = &self.redis {
            r.start(options).await?;
        }
        Ok(())
    }

    /// Drain both runtimes. Mode and timeout apply to both in sequence.
    #[napi]
    pub async fn shutdown(&self, options: Option<ShutdownOptions>) -> Result<()> {
        // Fire the abort handler exactly once — both backends' handlers were
        // registered through `EddyqApp.work`, which stored AbortControllers
        // under this instance's WeakMap entry. One broadcast covers both.
        let mode_str = options
            .as_ref()
            .and_then(|o| o.mode.as_deref())
            .unwrap_or("drain")
            .to_owned();
        if let Some(handler) = self.abort_handler.lock().expect("abort lock").as_ref() {
            handler.call(mode_str, ThreadsafeFunctionCallMode::NonBlocking);
        }
        if let Some(p) = &self.pg {
            let _ = p
                .shutdown(options.as_ref().map(clone_shutdown_options))
                .await;
        }
        if let Some(r) = &self.redis {
            let _ = r.shutdown(options).await;
        }
        Ok(())
    }
}

fn clone_start_options(o: &StartOptions) -> StartOptions {
    StartOptions {
        skip_migration_check: o.skip_migration_check,
        sweep_interval_ms: o.sweep_interval_ms,
        stale_after_ms: o.stale_after_ms,
        heartbeat_interval_ms: o.heartbeat_interval_ms,
        cleanup_interval_ms: o.cleanup_interval_ms,
        completed_retention_secs: o.completed_retention_secs,
        failed_retention_secs: o.failed_retention_secs,
        cancelled_retention_secs: o.cancelled_retention_secs,
        batch_retention_secs: o.batch_retention_secs,
        leader_lease_secs: o.leader_lease_secs,
        fetch_poll_interval_ms: o.fetch_poll_interval_ms,
        scheduler_interval_ms: o.scheduler_interval_ms,
    }
}

fn clone_shutdown_options(o: &ShutdownOptions) -> ShutdownOptions {
    ShutdownOptions {
        mode: o.mode.clone(),
        graceful_timeout_ms: o.graceful_timeout_ms,
    }
}

fn state_to_str(s: JobState) -> &'static str {
    match s {
        JobState::Pending => "pending",
        JobState::Running => "running",
        JobState::Completed => "completed",
        JobState::Failed => "failed",
        JobState::Scheduled => "scheduled",
        JobState::Cancelled => "cancelled",
    }
}

fn napi_filter_to_core(f: ListJobsFilter) -> eddyq_core::stats::ListJobsFilter {
    eddyq_core::stats::ListJobsFilter {
        queue: f.queue,
        state: f.state.as_deref().and_then(|s| match s {
            "pending" => Some(JobState::Pending),
            "running" => Some(JobState::Running),
            "completed" => Some(JobState::Completed),
            "failed" => Some(JobState::Failed),
            "scheduled" => Some(JobState::Scheduled),
            "cancelled" => Some(JobState::Cancelled),
            _ => None,
        }),
        kind: f.kind,
        group_key: f.group_key,
        tag: f.tag,
        id: f.id,
    }
}

fn job_row_to_napi(r: eddyq_core::stats::JobRow) -> JobRow {
    JobRow {
        id: r.id,
        queue: r.queue,
        kind: r.kind,
        state: r.state,
        priority: r.priority,
        attempt: r.attempt,
        max_attempts: r.max_attempts,
        scheduled_at: r.scheduled_at.to_rfc3339(),
        created_at: r.created_at.to_rfc3339(),
        finalized_at: r.finalized_at.map(|t| t.to_rfc3339()),
        group_key: r.group_key,
        tags: r.tags,
        payload: r.payload,
        result: r.result,
        errors: r.errors,
        metadata: r.metadata,
    }
}

fn group_to_napi(g: eddyq_core::group::Group) -> Group {
    Group {
        key: g.key,
        running_count: g.running_count,
        max_concurrency: g.max_concurrency,
        paused: g.paused,
        rate_count: g.rate_count,
        rate_period_ms: g.rate_period_ms,
        tokens: g.tokens,
        tokens_refilled_at: g.tokens_refilled_at.map(|t| t.to_rfc3339()),
        created_at: g.created_at.to_rfc3339(),
        updated_at: g.updated_at.to_rfc3339(),
    }
}

fn nq_to_napi(q: eddyq_core::named_queue::NamedQueue) -> NamedQueue {
    NamedQueue {
        name: q.name,
        running_count: q.running_count,
        max_concurrency: q.max_concurrency,
        paused: q.paused,
        default_timeout_ms: q.default_timeout_ms,
        created_at: q.created_at.to_rfc3339(),
        updated_at: q.updated_at.to_rfc3339(),
    }
}

fn schedule_to_napi(s: eddyq_core::schedule::Schedule) -> Schedule {
    Schedule {
        name: s.name,
        kind: s.kind,
        payload: s.payload,
        cron_expr: s.cron_expr,
        next_run_at: s.next_run_at.to_rfc3339(),
        last_run_at: s.last_run_at.map(|t| t.to_rfc3339()),
        enabled: s.enabled,
        priority: s.priority,
        max_attempts: s.max_attempts,
        queue: s.queue,
    }
}
