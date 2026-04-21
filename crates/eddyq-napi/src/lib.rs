//! eddyq-napi — Node.js bindings for `eddyq-client`.
//!
//! Built into a cdylib consumed by the `@eddyq/queue` npm package via NAPI-RS.
//! This crate exposes the **enqueue and admin** surface — workers (JS handler
//! callbacks) are not yet implemented; process jobs with `eddyq-cli` or a
//! Rust worker for now.

#![allow(clippy::missing_safety_doc)]

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use chrono::{DateTime, TimeZone, Utc};
use eddyq_client::{
    Client, ClientConfig, CoreQueue, CoreQueueBuilder, Directive, DynEnqueue, HandlerFailure,
    JobContext, JobResult, JobState,
};
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
    /// Max pool connections. Default 10.
    pub max_connections: Option<u32>,
    /// Min idle pool connections. Default 0.
    pub min_connections: Option<u32>,
    /// Acquire timeout in milliseconds. Default 30_000.
    pub acquire_timeout_ms: Option<u32>,
    /// Migration line name. Default "main".
    pub line: Option<String>,
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

/// Options for `eddyq.start()`.
#[napi(object)]
pub struct StartOptions {
    /// Skip the pending-migration check. Default `false` — `start()` errors
    /// out if any registered migration isn't applied, so you never boot
    /// workers against a stale schema.
    ///
    /// Set to `true` only when you've applied migrations via a separate
    /// deploy step (recommended) and don't want the boot-time check.
    pub skip_migration_check: Option<bool>,
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
}

/// Options for `addSchedule`. All optional — defaults match `enqueue`.
#[napi(object)]
pub struct ScheduleOptions {
    /// Priority (higher runs first). Default 0.
    pub priority: Option<i16>,
    /// Max total attempts before the job is marked failed. Default 3.
    pub max_attempts: Option<i32>,
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
        let cfg = build_cfg(options);
        let client = run(move || async move {
            Client::connect_with(&database_url, cfg).await.map_err(err)
        }).await?;
        Ok(Queue {
            client,
            state: Arc::new(Mutex::new(WorkerState::default())),
            abort_handler: Arc::new(Mutex::new(None)),
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
            client.set_group_concurrency(&group_key, max).await.map_err(err)
        }).await
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
            client.set_group_rate(&group_key, count, period).await.map_err(err)
        }).await
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
        run(move || async move {
            client.set_queue_concurrency(&queue, max).await.map_err(err)
        }).await
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
        run(move || async move {
            client.set_queue_timeout(&queue, timeout).await.map_err(err)
        }).await
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
        run(move || async move {
            client
                .add_schedule(&name, &cron_expr, &kind, payload, priority, max_attempts)
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
        let mut state = self.state.lock().expect("worker state lock poisoned");
        match &mut *state {
            WorkerState::Building { handlers, .. } => {
                handlers.push((kind, Arc::new(handler)));
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
        // would block app startup for every replica (River's model; proven
        // right in prod). If anything's pending, we refuse to start and tell
        // the operator how to fix it.
        let skip_check = options
            .as_ref()
            .and_then(|o| o.skip_migration_check)
            .unwrap_or(false);
        if !skip_check {
            let client = self.client.clone();
            let statuses = run(move || async move { client.migration_status().await.map_err(err) })
                .await?;
            let pending: Vec<_> = statuses.iter().filter(|s| s.applied_at.is_none()).collect();
            if !pending.is_empty() {
                let names: Vec<String> =
                    pending.iter().map(|p| format!("{}:{}", p.version, p.name)).collect();
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

        let mut builder = CoreQueueBuilder::new(self.client.pool().clone()).line(self.client.line());
        if let Some(n) = concurrency {
            builder = builder.worker_concurrency(n);
        }
        if let Some(qs) = subscribe {
            builder = builder.subscribe_to(qs);
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
        let mut slot = self.abort_handler.lock().expect("abort handler lock poisoned");
        *slot = Some(handler);
        Ok(())
    }

    /// Stop the worker runtime. Signals any registered abort handler first,
    /// then waits up to `gracefulTimeoutMs` (default 30 000) for in-flight
    /// jobs to finish before forcibly cancelling the runtime tasks. Admin
    /// methods remain usable after shutdown — call `close()` to release the
    /// DB pool entirely.
    #[napi]
    pub async fn shutdown(&self, graceful_timeout_ms: Option<u32>) -> Result<()> {
        let queue = {
            let mut state = self.state.lock().expect("worker state lock poisoned");
            match std::mem::replace(&mut *state, WorkerState::Stopped) {
                WorkerState::Running { queue } => queue,
                WorkerState::Building { .. } | WorkerState::Stopped => {
                    return Err(napi::Error::from_reason("Queue is not running"));
                }
            }
        };

        // Fire abort to JS so any in-flight handler's AbortSignal flips.
        // Held across `.call()` but it's NonBlocking — no await.
        {
            let guard = self.abort_handler.lock().expect("abort handler lock poisoned");
            if let Some(handler) = guard.as_ref() {
                handler.call("shutdown".to_owned(), ThreadsafeFunctionCallMode::NonBlocking);
            }
        }

        let grace = Duration::from_millis(u64::from(graceful_timeout_ms.unwrap_or(30_000)));
        let shutdown_fut = run(move || async move { queue.shutdown().await.map_err(err) });
        match tokio::time::timeout(grace, shutdown_fut).await {
            Ok(res) => res,
            Err(_) => Err(napi::Error::from_reason(format!(
                "shutdown exceeded graceful timeout ({:?}) — runtime tasks still in flight",
                grace
            ))),
        }
    }

    // --- Lifecycle --------------------------------------------------------

    /// Close the underlying Postgres pool. Call on shutdown.
    #[napi]
    pub async fn close(&self) -> Result<()> {
        let client = self.client.clone();
        run(move || async move {
            client.close().await;
            Ok(())
        }).await
    }
}

/// Build the `Handler` closure that bridges eddyq-core's dispatch to a JS
/// ThreadsafeFunction. Each call: (a) hops to the Node main thread with the
/// `JobCall`, (b) receives the Promise the handler returned, (c) awaits the
/// Promise's resolved value. A rejection is surfaced as a JobResult error so
/// the core runtime's retry/fail logic kicks in.
fn dispatcher(
    tsfn: JsHandler,
) -> impl Fn(serde_json::Value, JobContext)
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
        if let Some(n) = opts.max_attempts { req.max_attempts = n; }
        if let Some(p) = opts.priority { req.priority = p; }
        if let Some(q) = opts.queue { req.queue = q; }
        if opts.scheduled_at_ms.is_some() && opts.delay_ms.is_some() {
            return Err(napi::Error::from_reason(
                "enqueue: pass either scheduledAtMs or delayMs, not both",
            ));
        }
        if let Some(ms) = opts.scheduled_at_ms { req.scheduled_at = Some(ms_to_utc(ms)); }
        if let Some(ms) = opts.delay_ms {
            req.scheduled_at = Some(Utc::now() + chrono::Duration::milliseconds(ms));
        }
        if let Some(k) = opts.unique_key { req.unique_key = Some(k); }
        if let Some(g) = opts.group_key { req.group_key = Some(g); }
        if let Some(t) = opts.tags { req.tags = t; }
        if let Some(m) = opts.metadata { req.metadata = m; }
    }
    let result = client.enqueue(req).await.map_err(err)?;
    Ok(match result {
        eddyq_client::EnqueueResult::Inserted(id) => EnqueueOutcome { inserted: true, id: Some(id) },
        eddyq_client::EnqueueResult::Skipped => EnqueueOutcome { inserted: false, id: None },
    })
}

async fn do_cancel(client: Client, id: i64) -> Result<bool> {
    client.cancel(id).await.map_err(err)
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
        })
        .collect())
}

// -- Plain helpers -----------------------------------------------------------

fn build_cfg(options: Option<ConnectOptions>) -> ClientConfig {
    let mut c = ClientConfig::default();
    let Some(o) = options else { return c };
    if let Some(n) = o.max_connections { c.max_connections = n; }
    if let Some(n) = o.min_connections { c.min_connections = n; }
    if let Some(ms) = o.acquire_timeout_ms {
        c.acquire_timeout = Duration::from_millis(u64::from(ms));
    }
    if let Some(line) = o.line { c.line = line; }
    c
}

fn ms_to_utc(ms: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(ms).single().unwrap_or_else(Utc::now)
}

fn report_to_dto(report: &eddyq_client::MigrateReport) -> MigrateReport {
    MigrateReport {
        applied: report.applied.iter().map(|(v, n)| MigrationRow {
            version: *v,
            name: (*n).to_owned(),
        }).collect(),
        rolled_back: report.rolled_back.iter().map(|(v, n)| MigrationRow {
            version: *v,
            name: (*n).to_owned(),
        }).collect(),
    }
}
