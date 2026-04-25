//! eddyq-client — enqueue and admin API for eddyq.
//!
//! Thin wrapper around `eddyq-core` that owns a Postgres pool and exposes the
//! surface language bindings (NAPI, future Python/Go) consume: enqueue jobs by
//! string `kind`, cancel, migrate, and manage groups/named queues.
//!
//! Rust apps that want *workers* and compile-time-typed handlers should use
//! `eddyq-core::Queue` directly; this crate is the non-handler facade.

#![forbid(unsafe_code)]

use std::time::Duration;

use sqlx::{PgPool, postgres::PgPoolOptions};

pub use eddyq_core::{
    BulkEnqueueResult, Directive, DynEnqueue, EnqueueResult, Error, HandlerFailure, JobContext,
    JobId, JobResult, JobState, Queue as CoreQueue, QueueBuilder as CoreQueueBuilder, QueueConfig,
    Result,
    group::{Group, GroupRule, StoredRule},
    migrate::{Direction, MigrateReport, MigrationStatus},
    named_queue::NamedQueue,
    schedule::{Schedule, ScheduleDeclaration, SyncReport},
    stats::{JobList, JobRow, JobStats, ListJobsFilter, Pagination, QueueStateCount},
};

/// High-level client: owns a `PgPool` and delegates to eddyq-core.
#[derive(Debug, Clone)]
pub struct Client {
    pool: PgPool,
    line: String,
}

/// Builder configuration for `Client::connect_with`.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub max_connections: u32,
    pub min_connections: u32,
    pub acquire_timeout: Duration,
    pub line: String,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            // 5 is intentionally conservative. Job handlers don't hold
            // connections — they're only needed briefly for fetch, heartbeat,
            // and complete/fail. At N pods this becomes N×(max_connections+1)
            // connections total (the +1 is the LISTEN socket). Size explicitly
            // for your fleet rather than relying on this default.
            max_connections: 5,
            min_connections: 0,
            acquire_timeout: Duration::from_secs(30),
            line: eddyq_core::migrate::DEFAULT_LINE.to_owned(),
        }
    }
}

impl Client {
    /// Connect using sensible defaults (pool size 5, line `"main"`).
    pub async fn connect(database_url: &str) -> Result<Self> {
        Self::connect_with(database_url, ClientConfig::default()).await
    }

    pub async fn connect_with(database_url: &str, cfg: ClientConfig) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(cfg.max_connections)
            .min_connections(cfg.min_connections)
            .acquire_timeout(cfg.acquire_timeout)
            .connect(database_url)
            .await?;
        Ok(Self {
            pool,
            line: cfg.line,
        })
    }

    /// Wrap an existing pool (e.g. if the host app already manages one).
    pub fn from_pool(pool: PgPool) -> Self {
        Self {
            pool,
            line: eddyq_core::migrate::DEFAULT_LINE.to_owned(),
        }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub fn line(&self) -> &str {
        &self.line
    }

    pub async fn close(self) {
        self.pool.close().await;
    }

    // --- Migrations --------------------------------------------------------

    pub async fn migrate(&self) -> Result<MigrateReport> {
        eddyq_core::migrate::up(&self.pool, &self.line).await
    }

    pub async fn migrate_down(&self, max_steps: usize) -> Result<MigrateReport> {
        eddyq_core::migrate::down(&self.pool, &self.line, max_steps).await
    }

    pub async fn migration_status(&self) -> Result<Vec<MigrationStatus>> {
        eddyq_core::migrate::status(&self.pool, &self.line).await
    }

    // --- Enqueue -----------------------------------------------------------

    pub async fn enqueue(&self, req: DynEnqueue) -> Result<EnqueueResult> {
        eddyq_core::enqueue::enqueue_dyn(&self.pool, req).await
    }

    /// Bulk enqueue — a single UNNEST INSERT, dramatically faster than calling
    /// `enqueue` N times. Supports mixed `kind` across the batch.
    pub async fn enqueue_many(&self, reqs: Vec<DynEnqueue>) -> Result<BulkEnqueueResult> {
        eddyq_core::enqueue::enqueue_many_dyn(&self.pool, reqs).await
    }

    pub async fn enqueue_in_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        req: DynEnqueue,
    ) -> Result<EnqueueResult> {
        eddyq_core::enqueue::enqueue_dyn_in_tx(tx, req).await
    }

    // --- Cancel ------------------------------------------------------------

    pub async fn cancel(&self, id: JobId) -> Result<bool> {
        eddyq_core::fetch::cancel(&self.pool, id).await
    }

    // --- Group admin -------------------------------------------------------

    pub async fn set_group_concurrency(&self, key: &str, max: i32) -> Result<()> {
        eddyq_core::group::set_concurrency(&self.pool, key, max).await
    }

    pub async fn pause_group(&self, key: &str) -> Result<()> {
        eddyq_core::group::set_paused(&self.pool, key, true).await
    }

    pub async fn resume_group(&self, key: &str) -> Result<()> {
        eddyq_core::group::set_paused(&self.pool, key, false).await
    }

    pub async fn set_group_rate(&self, key: &str, count: u32, period: Duration) -> Result<()> {
        eddyq_core::group::set_rate(&self.pool, key, count, period).await
    }

    pub async fn clear_group_rate(&self, key: &str) -> Result<()> {
        eddyq_core::group::clear_rate(&self.pool, key).await
    }

    pub async fn get_group(&self, key: &str) -> Result<Option<Group>> {
        eddyq_core::group::get(&self.pool, key).await
    }

    pub async fn list_groups(&self) -> Result<Vec<Group>> {
        eddyq_core::group::list(&self.pool).await
    }

    pub async fn set_group_rule(&self, pattern: &str, rule: GroupRule) -> Result<()> {
        eddyq_core::group::set_rule(&self.pool, pattern, rule).await
    }

    pub async fn remove_group_rule(&self, pattern: &str) -> Result<bool> {
        eddyq_core::group::remove_rule(&self.pool, pattern).await
    }

    pub async fn list_group_rules(&self) -> Result<Vec<StoredRule>> {
        eddyq_core::group::list_rules(&self.pool).await
    }

    // --- Named-queue admin -------------------------------------------------

    pub async fn set_queue_concurrency(&self, name: &str, max: i32) -> Result<()> {
        eddyq_core::named_queue::set_concurrency(&self.pool, name, max).await
    }

    pub async fn pause_queue(&self, name: &str) -> Result<()> {
        eddyq_core::named_queue::set_paused(&self.pool, name, true).await
    }

    pub async fn resume_queue(&self, name: &str) -> Result<()> {
        eddyq_core::named_queue::set_paused(&self.pool, name, false).await
    }

    pub async fn set_queue_timeout(&self, name: &str, timeout: Option<Duration>) -> Result<()> {
        eddyq_core::named_queue::set_timeout(&self.pool, name, timeout).await
    }

    pub async fn get_queue(&self, name: &str) -> Result<Option<NamedQueue>> {
        eddyq_core::named_queue::get(&self.pool, name).await
    }

    pub async fn list_named_queues(&self) -> Result<Vec<NamedQueue>> {
        eddyq_core::named_queue::list(&self.pool).await
    }

    // --- Schedules ---------------------------------------------------------

    pub async fn list_schedules(&self) -> Result<Vec<Schedule>> {
        eddyq_core::schedule::list_schedules(&self.pool).await
    }

    /// Upsert a cron schedule. Passing the same `name` updates the existing
    /// schedule's cron/payload/priority and resets `next_run_at` accordingly.
    pub async fn add_schedule(
        &self,
        name: &str,
        cron_expr: &str,
        kind: &str,
        payload: serde_json::Value,
        priority: i16,
        max_attempts: i32,
    ) -> Result<()> {
        eddyq_core::schedule::upsert_schedule_raw(
            &self.pool,
            name,
            cron_expr,
            kind,
            payload,
            priority,
            max_attempts,
        )
        .await
    }

    pub async fn remove_schedule(&self, name: &str) -> Result<bool> {
        eddyq_core::schedule::remove_schedule(&self.pool, name).await
    }

    pub async fn set_schedule_enabled(&self, name: &str, enabled: bool) -> Result<bool> {
        eddyq_core::schedule::set_enabled(&self.pool, name, enabled).await
    }

    /// Reconcile DB schedules against a code-declared list. Upserts each entry
    /// and deletes any DB schedule whose name is not in the list. Idempotent.
    pub async fn sync_schedules(
        &self,
        declared: &[eddyq_core::schedule::ScheduleDeclaration],
    ) -> Result<eddyq_core::schedule::SyncReport> {
        eddyq_core::schedule::sync_schedules(&self.pool, declared).await
    }

    // --- Stats / list-jobs -------------------------------------------------

    pub async fn get_stats(&self) -> Result<JobStats> {
        eddyq_core::stats::get_stats(&self.pool).await
    }

    pub async fn list_jobs(
        &self,
        filter: ListJobsFilter,
        pagination: Pagination,
    ) -> Result<JobList> {
        eddyq_core::stats::list_jobs(&self.pool, filter, pagination).await
    }
}
