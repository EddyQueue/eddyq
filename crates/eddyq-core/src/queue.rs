use std::{sync::Arc, time::Duration};

use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::{
    backend::{Backend, CleanState, PgBackend},
    enqueue::{EnqueueOptions, EnqueueResult},
    error::{Error, Result},
    job::Job,
    runtime::{self, RuntimeHandles},
    worker::{Worker, WorkerRegistry},
};

#[derive(Debug, Clone)]
pub struct QueueConfig {
    pub fetch_poll_interval: Duration,
    pub fetch_cooldown: Duration,
    pub fetch_batch_size: usize,
    pub worker_concurrency: usize,
    pub heartbeat_interval: Duration,
    pub sweep_interval: Duration,
    pub stale_after: Duration,
    pub retry_base: Duration,
    pub retry_max: Duration,
    pub scheduler_interval: Duration,
    /// How often the cleanup task runs.
    pub cleanup_interval: Duration,
    /// Delete completed jobs older than this. `None` = keep forever.
    /// Default: 24h.
    pub completed_retention: Option<Duration>,
    /// Delete failed jobs older than this. `None` = keep forever.
    /// Default: 7 days.
    pub failed_retention: Option<Duration>,
    /// Delete cancelled jobs older than this. `None` = keep forever.
    /// Default: 7 days.
    pub cancelled_retention: Option<Duration>,
    /// Delete finalized batch rows (`eddyq_batches`) older than this. `None`
    /// keeps them forever (table grows unbounded for batch-heavy workloads).
    /// Default: 7 days.
    pub batch_retention: Option<Duration>,
    /// When `true`, do not spawn a LISTEN/NOTIFY listener. Use this when connected
    /// through PgBouncer in transaction-pooling mode (LISTEN is incompatible).
    pub poll_only: bool,
    /// Leader election lease duration in seconds. The elected leader refreshes
    /// its lease every `leader_lease_secs / 3` seconds. Default: 30.
    pub leader_lease_secs: u64,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            fetch_poll_interval: Duration::from_secs(1),
            fetch_cooldown: Duration::from_millis(100),
            fetch_batch_size: 10,
            worker_concurrency: 10,
            heartbeat_interval: Duration::from_secs(15),
            sweep_interval: Duration::from_secs(30),
            stale_after: Duration::from_secs(60),
            retry_base: Duration::from_secs(1),
            retry_max: Duration::from_secs(300),
            scheduler_interval: Duration::from_secs(5),
            cleanup_interval: Duration::from_secs(300), // 5 min
            completed_retention: Some(Duration::from_secs(24 * 60 * 60)), // 24h
            failed_retention: Some(Duration::from_secs(7 * 24 * 60 * 60)), // 7d
            cancelled_retention: Some(Duration::from_secs(7 * 24 * 60 * 60)), // 7d
            batch_retention: Some(Duration::from_secs(7 * 24 * 60 * 60)), // 7d
            poll_only: false,
            leader_lease_secs: 30,
        }
    }
}

pub struct QueueBuilder<B: Backend = PgBackend> {
    backend: B,
    registry: WorkerRegistry,
    config: QueueConfig,
    line: String,
    queues: Vec<String>,
}

impl QueueBuilder<PgBackend> {
    /// Build a Postgres-backed queue. Equivalent to
    /// `QueueBuilder::with_backend(PgBackend::new(pool))` — kept as the
    /// primary constructor since Postgres is the default backend.
    pub fn new(pool: PgPool) -> Self {
        Self::with_backend(PgBackend::new(pool))
    }
}

impl<B: Backend> QueueBuilder<B> {
    /// Build a queue around any `Backend` impl. Use this for non-default
    /// backends (e.g. `RedisBackend`) or when you've constructed a
    /// `PgBackend` explicitly.
    pub fn with_backend(backend: B) -> Self {
        Self {
            backend,
            registry: WorkerRegistry::new(),
            config: QueueConfig::default(),
            line: crate::migrate::DEFAULT_LINE.to_owned(),
            queues: vec![crate::job::DEFAULT_QUEUE.to_owned()],
        }
    }

    /// Subscribe this queue's workers to specific named queues. Default is
    /// `["default"]`. Use named queues to split worker pools — e.g., one
    /// process on `["urgent"]` and another on `["default", "low"]`.
    pub fn subscribe_to<I, S>(mut self, queues: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.queues = queues.into_iter().map(Into::into).collect();
        if self.queues.is_empty() {
            self.queues.push(crate::job::DEFAULT_QUEUE.to_owned());
        }
        self
    }

    /// Name the migration line this queue uses. Default is `"main"`. Use
    /// distinct lines when you want multiple logical eddyq instances to track
    /// their migration histories separately. Lines do not isolate tables —
    /// for that, use separate Postgres schemas or databases.
    pub fn line(mut self, name: impl Into<String>) -> Self {
        self.line = name.into();
        self
    }

    pub fn register<J, W>(mut self, worker: W) -> Self
    where
        J: Job,
        W: Worker<J> + 'static,
    {
        self.registry.register::<J, W>(worker);
        self
    }

    /// Register a handler keyed by string `kind` (no `Job` trait required).
    /// Used by language bindings where the handler is a foreign function.
    pub fn register_dyn<F, Fut>(mut self, kind: impl Into<String>, f: F) -> Self
    where
        F: Fn(serde_json::Value, crate::job::JobContext) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = crate::error::JobResult<serde_json::Value>>
            + Send
            + 'static,
    {
        self.registry.register_dyn(kind, f);
        self
    }

    pub fn config(mut self, config: QueueConfig) -> Self {
        self.config = config;
        self
    }

    pub fn worker_concurrency(mut self, n: usize) -> Self {
        self.config.worker_concurrency = n.max(1);
        self
    }

    pub fn fetch_poll_interval(mut self, d: Duration) -> Self {
        self.config.fetch_poll_interval = d;
        self
    }

    pub fn poll_only(mut self, yes: bool) -> Self {
        self.config.poll_only = yes;
        self
    }

    pub fn stale_after(mut self, d: Duration) -> Self {
        self.config.stale_after = d;
        self
    }

    pub fn heartbeat_interval(mut self, d: Duration) -> Self {
        self.config.heartbeat_interval = d;
        self
    }

    pub fn sweep_interval(mut self, d: Duration) -> Self {
        self.config.sweep_interval = d;
        self
    }

    pub fn cleanup_interval(mut self, d: Duration) -> Self {
        self.config.cleanup_interval = d;
        self
    }

    /// How often the leader scheduler loop fires due schedules + promotes
    /// delayed jobs. Default 5s. Lower values make interval schedules
    /// (`{ every: ms }` on Redis) more responsive.
    pub fn scheduler_interval(mut self, d: Duration) -> Self {
        self.config.scheduler_interval = d;
        self
    }

    /// Retention for completed jobs. `None` keeps them forever.
    pub fn completed_retention(mut self, d: Option<Duration>) -> Self {
        self.config.completed_retention = d;
        self
    }

    /// Retention for failed jobs. `None` keeps them forever.
    pub fn failed_retention(mut self, d: Option<Duration>) -> Self {
        self.config.failed_retention = d;
        self
    }

    /// Retention for cancelled jobs. `None` keeps them forever.
    pub fn cancelled_retention(mut self, d: Option<Duration>) -> Self {
        self.config.cancelled_retention = d;
        self
    }

    /// Retention for finalized batch rows. `None` keeps them forever — the
    /// `eddyq_batches` table grows unbounded for batch-heavy workloads.
    pub fn batch_retention(mut self, d: Option<Duration>) -> Self {
        self.config.batch_retention = d;
        self
    }

    pub fn leader_lease_secs(mut self, s: u64) -> Self {
        self.config.leader_lease_secs = s;
        self
    }

    pub fn build(self) -> Queue<B> {
        Queue {
            backend: Arc::new(self.backend),
            registry: Arc::new(self.registry),
            config: self.config,
            line: self.line,
            queues: self.queues,
            state: std::sync::Mutex::new(QueueState::Idle),
        }
    }
}

enum QueueState {
    Idle,
    Running {
        shutdown: CancellationToken,
        handles: RuntimeHandles,
    },
}

pub struct Queue<B: Backend = PgBackend> {
    backend: Arc<B>,
    registry: Arc<WorkerRegistry>,
    config: QueueConfig,
    line: String,
    queues: Vec<String>,
    state: std::sync::Mutex<QueueState>,
}

impl Queue<PgBackend> {
    /// Build a Postgres-backed queue from a `PgPool`. Preserves the original
    /// API; equivalent to `Queue::with_backend(PgBackend::new(pool))`.
    pub fn builder(pool: PgPool) -> QueueBuilder<PgBackend> {
        QueueBuilder::<PgBackend>::new(pool)
    }

    /// Direct access to the Postgres pool. Postgres-only. Used by the
    /// `hello.rs` example and by callers that want to issue raw SQL
    /// alongside their queue work.
    pub fn pool(&self) -> &PgPool {
        self.backend.pool()
    }

    // ---- Postgres-only inherent methods ----------------------------------

    /// Apply all pending schema migrations for this queue's line.
    pub async fn migrate(&self) -> Result<crate::migrate::MigrateReport> {
        self.backend.migrate_up(&self.line).await
    }

    pub async fn migrate_down(&self, max_steps: usize) -> Result<crate::migrate::MigrateReport> {
        self.backend.migrate_down(&self.line, max_steps).await
    }

    pub async fn migration_status(&self) -> Result<Vec<crate::migrate::MigrationStatus>> {
        self.backend.migration_status(&self.line).await
    }

    /// Enqueue a job inside the caller's transaction. The job row is only
    /// visible to workers if the user's transaction commits. On rollback the
    /// job — and any follow-on NOTIFY — are discarded. **Postgres only.**
    pub async fn enqueue_in_tx<J: Job>(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        job: &J,
    ) -> Result<EnqueueResult> {
        self.backend
            .enqueue_in_tx(tx, job, EnqueueOptions::default())
            .await
    }

    pub async fn enqueue_in_tx_with<J: Job>(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        job: &J,
        opts: EnqueueOptions,
    ) -> Result<EnqueueResult> {
        self.backend.enqueue_in_tx(tx, job, opts).await
    }

    /// Transactional bulk enqueue. All or nothing — rolls back with the user's tx.
    /// **Postgres only.**
    pub async fn enqueue_many_in_tx<J: Job>(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        jobs: &[J],
    ) -> Result<crate::enqueue::BulkEnqueueResult> {
        self.backend.enqueue_many_in_tx(tx, jobs).await
    }
}

impl<B: Backend> Queue<B> {
    /// The migration line this queue was built for (default: `"main"`).
    pub fn line(&self) -> &str {
        &self.line
    }

    /// Underlying backend (e.g. for capability inspection or backend-specific
    /// admin calls).
    pub fn backend(&self) -> &Arc<B> {
        &self.backend
    }

    /// Backend capability flags — what this queue can/can't do at runtime.
    pub fn caps(&self) -> crate::backend::BackendCaps {
        self.backend.caps()
    }

    pub async fn enqueue<J: Job>(&self, job: &J) -> Result<EnqueueResult> {
        let req = build_dyn_enqueue(job, EnqueueOptions::default())?;
        self.backend.enqueue(req).await
    }

    pub async fn enqueue_with<J: Job>(
        &self,
        job: &J,
        opts: EnqueueOptions,
    ) -> Result<EnqueueResult> {
        let req = build_dyn_enqueue(job, opts)?;
        self.backend.enqueue(req).await
    }

    /// Bulk-enqueue N jobs of the same kind in a single round-trip. Much
    /// faster than calling `enqueue` in a loop for large batches. Returns an
    /// aggregate count (inserted + skipped-via-unique-conflict); for per-row
    /// results, use `enqueue` in a loop.
    pub async fn enqueue_many<J: Job>(
        &self,
        jobs: &[J],
    ) -> Result<crate::enqueue::BulkEnqueueResult> {
        let mut reqs = Vec::with_capacity(jobs.len());
        for j in jobs {
            reqs.push(build_dyn_enqueue(j, EnqueueOptions::default())?);
        }
        self.backend.enqueue_many(reqs).await
    }

    /// Cancel a pending job by id. Returns `true` if cancelled, `false` if
    /// the job doesn't exist or is already running / finalized. Can't cancel
    /// a running job — the handler must cooperate for that.
    pub async fn cancel(&self, id: crate::job::JobId) -> Result<bool> {
        self.backend.cancel(id).await
    }

    /// Register or update a recurring schedule. Jobs will be auto-enqueued when
    /// each cron occurrence is due. Skip-missed semantics: one enqueue per tick,
    /// regardless of how many runs were missed while the scheduler was down.
    pub async fn add_schedule<J: Job>(&self, name: &str, cron_expr: &str, job: &J) -> Result<()> {
        let payload = serde_json::to_value(job)?;
        self.backend
            .upsert_schedule_raw(
                name,
                cron_expr,
                J::KIND,
                payload,
                job.priority(),
                job.max_attempts(),
                job.queue(),
            )
            .await
    }

    /// Register or update an interval-driven schedule (BullMQ `{ every: ms }`).
    /// Fires every `interval_ms` from the moment the previous fire landed.
    /// Skip-missed semantics match `add_schedule`: a delayed leader doesn't
    /// catch up.
    pub async fn add_interval_schedule<J: Job>(
        &self,
        name: &str,
        interval: Duration,
        job: &J,
    ) -> Result<()> {
        let payload = serde_json::to_value(job)?;
        let interval_ms = i64::try_from(interval.as_millis())
            .map_err(|_| Error::InvalidArgument("interval too large".into()))?;
        self.backend
            .upsert_interval_schedule_raw(
                name,
                interval_ms,
                J::KIND,
                payload,
                job.priority(),
                job.max_attempts(),
                job.queue(),
            )
            .await
    }

    pub async fn remove_schedule(&self, name: &str) -> Result<bool> {
        self.backend.remove_schedule(name).await
    }

    pub async fn set_schedule_enabled(&self, name: &str, enabled: bool) -> Result<bool> {
        self.backend.set_schedule_enabled(name, enabled).await
    }

    pub async fn list_schedules(&self) -> Result<Vec<crate::schedule::Schedule>> {
        self.backend.list_schedules().await
    }

    /// Reconcile DB schedules against a code-declared list. Each entry is
    /// upserted; any DB schedule not in the list is deleted. Idempotent.
    pub async fn sync_schedules(
        &self,
        declared: &[crate::schedule::ScheduleDeclaration],
    ) -> Result<crate::schedule::SyncReport> {
        self.backend.sync_schedules(declared).await
    }

    /// Set the concurrency cap for a group. Jobs with `group_key(key)` will not
    /// run more than `max` at a time.
    pub async fn set_group_concurrency(&self, key: &str, max: i32) -> Result<()> {
        self.backend.group_set_concurrency(key, max).await
    }

    pub async fn pause_group(&self, key: &str) -> Result<()> {
        self.backend.group_set_paused(key, true).await
    }

    pub async fn resume_group(&self, key: &str) -> Result<()> {
        self.backend.group_set_paused(key, false).await
    }

    pub async fn get_group(&self, key: &str) -> Result<Option<crate::group::Group>> {
        self.backend.group_get(key).await
    }

    pub async fn list_groups(&self) -> Result<Vec<crate::group::Group>> {
        self.backend.group_list().await
    }

    /// Set a throughput rate limit: at most `count` jobs may *start* per `period`
    /// for this group. Independent of `max_concurrency` — both constraints apply.
    /// Useful for external-API rate limits (e.g. 1000 req/min for OpenAI).
    pub async fn set_group_rate(&self, key: &str, count: u32, period: Duration) -> Result<()> {
        self.backend.group_set_rate(key, count, period).await
    }

    pub async fn clear_group_rate(&self, key: &str) -> Result<()> {
        self.backend.group_clear_rate(key).await
    }

    // --- Pattern-based group rules -----------------------------------------

    /// Register a default-values rule for a group-key glob pattern. Any group
    /// whose key matches this pattern will be auto-configured with these
    /// defaults on its first `enqueue()`, unless you've already explicitly
    /// called `set_group_concurrency` / `set_group_rate` for that specific key.
    ///
    /// Patterns use `*` (any chars) and `?` (one char).
    pub async fn set_group_rule(&self, pattern: &str, rule: crate::group::GroupRule) -> Result<()> {
        self.backend.group_set_rule(pattern, rule).await
    }

    pub async fn remove_group_rule(&self, pattern: &str) -> Result<bool> {
        self.backend.group_remove_rule(pattern).await
    }

    pub async fn list_group_rules(&self) -> Result<Vec<crate::group::StoredRule>> {
        self.backend.group_list_rules().await
    }

    // --- Named-queue cross-process concurrency -----------------------------

    /// Cap the total concurrency of a named queue *across all worker
    /// processes*. Unlike `worker_concurrency` (which is per-process), this
    /// is a global cap enforced via a shared counter.
    pub async fn set_queue_concurrency(&self, name: &str, max: i32) -> Result<()> {
        self.backend.queue_set_concurrency(name, max).await
    }

    pub async fn pause_queue(&self, name: &str) -> Result<()> {
        self.backend.queue_set_paused(name, true).await
    }

    pub async fn resume_queue(&self, name: &str) -> Result<()> {
        self.backend.queue_set_paused(name, false).await
    }

    pub async fn get_queue(&self, name: &str) -> Result<Option<crate::named_queue::NamedQueue>> {
        self.backend.queue_get(name).await
    }

    pub async fn list_named_queues(&self) -> Result<Vec<crate::named_queue::NamedQueue>> {
        self.backend.queue_list().await
    }

    // --- Stats / read-only queries ----------------------------------------

    /// One-shot snapshot of job counts grouped by (queue, state). Single round
    /// trip — suitable as the landing query for a dashboard.
    pub async fn get_stats(&self) -> Result<crate::stats::JobStats> {
        self.backend.get_stats().await
    }

    /// Paginated job listing with optional filters. Ordered newest-first.
    pub async fn list_jobs(
        &self,
        filter: crate::stats::ListJobsFilter,
        pagination: crate::stats::Pagination,
    ) -> Result<crate::stats::JobList> {
        self.backend.list_jobs(filter, pagination).await
    }

    /// Set a default per-job timeout for jobs in this queue. Handlers that
    /// don't return within the duration are aborted and the job is marked
    /// failed (with retry if under `max_attempts`). Pass `None` to clear.
    pub async fn set_queue_timeout(&self, name: &str, timeout: Option<Duration>) -> Result<()> {
        self.backend.queue_set_timeout(name, timeout).await
    }

    /// Ad-hoc retention sweep — BullMQ `queue.clean()`. Deletes up to `limit`
    /// finalized jobs in `state` that are older than `grace`. Useful for
    /// one-shot pruning from admin tools or scripts; routine retention
    /// should go through the configured cleanup tick instead.
    pub async fn clean(&self, grace: Duration, limit: u32, state: CleanState) -> Result<u64> {
        self.backend.clean(grace, limit, state).await
    }

    pub fn start(&self) -> Result<()> {
        let mut state = self.state.lock().expect("queue state lock poisoned");
        if matches!(*state, QueueState::Running { .. }) {
            return Err(Error::AlreadyRunning);
        }

        let shutdown = CancellationToken::new();
        let handles = runtime::start(
            self.backend.clone(),
            self.registry.clone(),
            self.config.clone(),
            self.queues.clone(),
            shutdown.clone(),
        );

        info!(
            kinds = ?self.registry.kinds(),
            queues = ?self.queues,
            concurrency = self.config.worker_concurrency,
            backend = self.backend.caps().name,
            "eddyq queue started"
        );

        *state = QueueState::Running { shutdown, handles };
        Ok(())
    }

    /// Graceful shutdown: stop claiming new jobs, fire `AbortSignal` to
    /// in-flight handlers, await runtime tasks. Equivalent to
    /// `shutdown_with(ShutdownMode::Drain)`. Kept for back-compat.
    pub async fn shutdown(&self) -> Result<()> {
        self.shutdown_with(ShutdownMode::Drain).await
    }

    /// Stop the runtime in one of three modes — see [`ShutdownMode`] for the
    /// tradeoffs. The state transitions to `Idle` regardless of mode (you
    /// cannot resume a stopped Queue; build a new one).
    pub async fn shutdown_with(&self, mode: ShutdownMode) -> Result<()> {
        let (shutdown, handles) = {
            let mut state = self.state.lock().expect("queue state lock poisoned");
            match std::mem::replace(&mut *state, QueueState::Idle) {
                QueueState::Idle => return Err(Error::NotRunning),
                QueueState::Running { shutdown, handles } => (shutdown, handles),
            }
        };

        // Always fire the cancellation token first — runtime tasks (fetcher,
        // workers, sweeper, etc.) all check this and exit on next loop iter.
        // Handlers that captured the AbortSignal also see this fire.
        shutdown.cancel();

        match mode {
            ShutdownMode::Drain => {
                crate::runtime::await_all(handles).await;
                info!("eddyq queue stopped (drain)");
            }
            ShutdownMode::Force => {
                // Snapshot the in-flight set BEFORE we abort the workers, so
                // we don't miss jobs the worker was about to mark done.
                // Reclaim is filtered by `state = 'running'` so any rows the
                // workers manage to finalize between snapshot and reclaim are
                // safely skipped.
                let ids: Vec<crate::JobId> = handles
                    .in_flight
                    .lock()
                    .expect("in_flight lock poisoned")
                    .iter()
                    .copied()
                    .collect();
                handles.abort_all();
                let reclaimed = match self.backend.reclaim_in_flight(&ids).await {
                    Ok(n) => n,
                    Err(e) => {
                        // Best-effort. We've already aborted the runtime; if
                        // reclaim fails the heartbeat sweep on another pod
                        // will recover the rows after `stale_after`. Log and
                        // move on — don't fail the shutdown.
                        tracing::warn!(?e, "reclaim_in_flight failed during force shutdown");
                        0
                    }
                };
                drop(handles);
                info!(
                    snapshotted = ids.len(),
                    reclaimed, "eddyq queue stopped (force)"
                );
            }
            ShutdownMode::Abandon => {
                handles.abort_all();
                drop(handles);
                info!("eddyq queue stopped (abandon)");
            }
        }

        Ok(())
    }
}

/// Build a `DynEnqueue` from a typed `Job` + options. Centralizes the
/// trait-default lookup logic so both `enqueue` and `enqueue_with` share it.
fn build_dyn_enqueue<J: Job>(job: &J, opts: EnqueueOptions) -> Result<crate::enqueue::DynEnqueue> {
    let payload = serde_json::to_value(job)?;
    let mut req = crate::enqueue::DynEnqueue::new(J::KIND, payload);
    req.max_attempts = opts.max_attempts.unwrap_or_else(|| job.max_attempts());
    req.priority = opts.priority.unwrap_or_else(|| job.priority());
    req.scheduled_at = opts.scheduled_at;
    req.unique_key = opts.unique_key.or_else(|| job.unique_key());
    req.group_key = opts.group_key.or_else(|| job.group_key());
    req.tags = opts.tags.unwrap_or_else(|| job.tags());
    req.metadata = opts
        .metadata
        .or_else(|| job.metadata())
        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
    req.queue = opts.queue.unwrap_or_else(|| job.queue().to_owned());
    req.remove_on_complete = opts.remove_on_complete;
    req.remove_on_fail = opts.remove_on_fail;
    Ok(req)
}

/// How `Queue::shutdown_with` releases an in-flight worker pool. The right
/// choice depends on what's about to happen to the process:
///
/// * `Drain` — graceful. Stop claiming new jobs, fire `AbortSignal` to
///   in-flight handlers, then wait for them. Modeled on BullMQ's default
///   `worker.close()`. Use for routine deploys.
///
/// * `Force` — fast. Stop claiming, fire `AbortSignal`, abort runtime
///   tasks, then **proactively reclaim** rows this pod claimed (set
///   `running` → `pending`, attempt++) so other pods can pick them up
///   immediately. Modeled on BullMQ's `worker.close({ force: true })`
///   plus River's `StopAndCancel`. Use when the pod is about to be killed.
///
/// * `Abandon` — last-resort. Drop the runtime without reclaiming. Rows
///   stay in `running` until another pod's heartbeat sweep finds them
///   stale (one `stale_after` cycle later). Use only on panic exits where
///   you don't trust the pool/connection state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownMode {
    Drain,
    Force,
    Abandon,
}
