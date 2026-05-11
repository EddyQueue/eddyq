//! Postgres backend. Thin delegating impl over the existing module-level
//! functions in `enqueue`, `fetch`, `schedule`, `group`, `named_queue`,
//! `stats`, `leader`, and `migrate`.
//!
//! This delegation pattern (rather than moving every function in here as a
//! method) keeps the diff against current code small and lets us refactor
//! the PG modules incrementally later.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sqlx::PgPool;
use sqlx::postgres::PgListener;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use uuid::Uuid;

use super::{Backend, BackendCaps};
use crate::{
    enqueue::{self, BulkEnqueueResult, DynEnqueue, EnqueueResult},
    error::Result,
    fetch::{self, ClaimedJob, Retention},
    group::{self, Group, GroupRule, StoredRule},
    job::JobId,
    leader::{self, LEADER_RESIGN_CHANNEL},
    migrate::{self, MigrateReport, MigrationStatus},
    named_queue::{self, NamedQueue},
    schedule::{self, Schedule, ScheduleDeclaration, SyncReport},
    stats::{self, JobList, JobStats, ListJobsFilter, Pagination},
};

/// Postgres-backed implementation. Holds a `PgPool` and delegates trait
/// methods to the existing free functions in this crate.
#[derive(Debug, Clone)]
pub struct PgBackend {
    pool: PgPool,
}

impl PgBackend {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Direct access to the underlying pool. Escape hatch for advanced uses
    /// (raw queries, custom transactions). Prefer the typed methods below
    /// (`enqueue_in_tx`, `enqueue_many_in_tx`).
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    // -------- Postgres-only inherent methods (NOT on the Backend trait) ----
    //
    // These exist because Redis can't honor a user-side application
    // transaction. Putting them here means `Queue<RedisBackend>` cannot
    // accidentally call them — it's a compile error, not a runtime error.

    /// Enqueue inside the caller's transaction. The job row is only visible
    /// to workers if the user's transaction commits.
    pub async fn enqueue_in_tx<J: crate::job::Job>(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        job: &J,
        opts: enqueue::EnqueueOptions,
    ) -> Result<EnqueueResult> {
        enqueue::enqueue_in_tx(tx, job, opts).await
    }

    /// Bulk-enqueue inside the caller's transaction.
    pub async fn enqueue_many_in_tx<J: crate::job::Job>(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        jobs: &[J],
    ) -> Result<BulkEnqueueResult> {
        enqueue::enqueue_many_in_tx(tx, jobs).await
    }

    /// Dynamic-kind enqueue inside the caller's transaction. Used by
    /// language bindings.
    pub async fn enqueue_dyn_in_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        req: DynEnqueue,
    ) -> Result<EnqueueResult> {
        enqueue::enqueue_dyn_in_tx(tx, req).await
    }

    /// Bulk dynamic-kind enqueue inside the caller's transaction.
    pub async fn enqueue_many_dyn_in_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        reqs: Vec<DynEnqueue>,
    ) -> Result<BulkEnqueueResult> {
        enqueue::enqueue_many_dyn_in_tx(tx, reqs).await
    }

    // -------- migrations (Postgres-only inherent methods) -----------------
    //
    // Not on the `Backend` trait — Redis has no schema.

    pub async fn migrate_up(&self, line: &str) -> Result<MigrateReport> {
        migrate::up(&self.pool, line).await
    }

    pub async fn migrate_down(&self, line: &str, max_steps: usize) -> Result<MigrateReport> {
        migrate::down(&self.pool, line, max_steps).await
    }

    pub async fn migration_status(&self, line: &str) -> Result<Vec<MigrationStatus>> {
        migrate::status(&self.pool, line).await
    }
}

#[async_trait]
impl Backend for PgBackend {
    fn caps(&self) -> BackendCaps {
        BackendCaps {
            name: "postgres",
            transactional_enqueue: true,
            migrations: true,
            fast_wakeup: true,
            cancel_running: true,
            priority_range: (i16::MIN as i32, i16::MAX as i32),
            cluster_safe: false,
        }
    }

    // -------- enqueue ------------------------------------------------------

    async fn enqueue(&self, req: DynEnqueue) -> Result<EnqueueResult> {
        enqueue::enqueue_dyn(&self.pool, req).await
    }

    async fn enqueue_many(&self, reqs: Vec<DynEnqueue>) -> Result<BulkEnqueueResult> {
        enqueue::enqueue_many_dyn(&self.pool, reqs).await
    }

    // -------- worker runtime ----------------------------------------------

    async fn claim_batch(
        &self,
        worker_id: Uuid,
        batch_size: usize,
        kinds: &[String],
        queues: &[String],
    ) -> Result<Vec<ClaimedJob>> {
        fetch::claim_batch(&self.pool, worker_id, batch_size, kinds, queues).await
    }

    async fn update_heartbeat_batch(&self, ids: &[JobId]) -> Result<u64> {
        fetch::update_heartbeat_batch(&self.pool, ids).await
    }

    async fn mark_completed(
        &self,
        id: JobId,
        worker_id: Uuid,
        result: Option<serde_json::Value>,
    ) -> Result<()> {
        fetch::mark_completed(&self.pool, id, worker_id, result).await
    }

    async fn mark_failed(
        &self,
        id: JobId,
        worker_id: Uuid,
        error_entry: serde_json::Value,
        retry_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<()> {
        fetch::mark_failed(&self.pool, id, worker_id, error_entry, retry_at).await
    }

    async fn sweep_stale(&self, stale_after: Duration) -> Result<u64> {
        fetch::sweep_stale(&self.pool, stale_after).await
    }

    async fn cleanup(&self, retention: Retention) -> Result<(u64, u64, u64, u64)> {
        fetch::cleanup(&self.pool, retention).await
    }

    async fn reclaim_in_flight(&self, ids: &[JobId]) -> Result<u64> {
        fetch::reclaim_in_flight(&self.pool, ids).await
    }

    fn spawn_wakeup_listener(
        self: Arc<Self>,
        wakeup: Arc<Notify>,
        shutdown: CancellationToken,
    ) -> Option<JoinHandle<()>> {
        let pool = self.pool.clone();
        Some(tokio::spawn(async move {
            let mut listener = match PgListener::connect_with(&pool).await {
                Ok(l) => l,
                Err(err) => {
                    warn!(
                        ?err,
                        "LISTEN connection failed; falling back to polling only"
                    );
                    return;
                }
            };
            if let Err(err) = listener.listen(crate::runtime::NOTIFY_CHANNEL).await {
                warn!(?err, "LISTEN setup failed; falling back to polling only");
                return;
            }
            info!(
                channel = crate::runtime::NOTIFY_CHANNEL,
                "eddyq listener started"
            );
            loop {
                tokio::select! {
                    biased;
                    () = shutdown.cancelled() => break,
                    ev = listener.recv() => match ev {
                        Ok(_) => wakeup.notify_one(),
                        Err(err) => {
                            warn!(?err, "listener error, will continue");
                            tokio::time::sleep(Duration::from_millis(500)).await;
                        }
                    }
                }
            }
            info!("eddyq listener stopped");
        }))
    }

    // -------- leader -------------------------------------------------------

    async fn leader_try_elect(&self, worker_id: Uuid, role: &str, lease_secs: u64) -> Result<bool> {
        leader::try_elect(&self.pool, worker_id, role, lease_secs).await
    }

    async fn leader_resign(&self, worker_id: Uuid, role: &str) -> Result<()> {
        leader::resign(&self.pool, worker_id, role).await
    }

    fn spawn_leader_resign_listener(
        self: Arc<Self>,
        on_resign: Arc<Notify>,
        shutdown: CancellationToken,
    ) -> Option<JoinHandle<()>> {
        let pool = self.pool.clone();
        Some(tokio::spawn(async move {
            let mut listener = match PgListener::connect_with(&pool).await {
                Ok(l) => l,
                Err(err) => {
                    warn!(?err, "leader-resign LISTEN setup failed");
                    return;
                }
            };
            if let Err(err) = listener.listen(LEADER_RESIGN_CHANNEL).await {
                warn!(?err, "leader-resign LISTEN failed");
                return;
            }
            loop {
                tokio::select! {
                    biased;
                    () = shutdown.cancelled() => break,
                    ev = listener.recv() => match ev {
                        Ok(_) => on_resign.notify_one(),
                        Err(err) => {
                            warn!(?err, "leader-resign listener error");
                            tokio::time::sleep(Duration::from_millis(500)).await;
                        }
                    }
                }
            }
        }))
    }

    // -------- schedules ----------------------------------------------------

    async fn upsert_schedule_raw(
        &self,
        name: &str,
        cron_expr: &str,
        kind: &str,
        payload: serde_json::Value,
        priority: i16,
        max_attempts: i32,
        queue: &str,
    ) -> Result<()> {
        schedule::upsert_schedule_raw(
            &self.pool,
            name,
            cron_expr,
            kind,
            payload,
            priority,
            max_attempts,
            queue,
        )
        .await
    }

    async fn remove_schedule(&self, name: &str) -> Result<bool> {
        schedule::remove_schedule(&self.pool, name).await
    }

    async fn set_schedule_enabled(&self, name: &str, enabled: bool) -> Result<bool> {
        schedule::set_enabled(&self.pool, name, enabled).await
    }

    async fn list_schedules(&self) -> Result<Vec<Schedule>> {
        schedule::list_schedules(&self.pool).await
    }

    async fn sync_schedules(&self, declared: &[ScheduleDeclaration]) -> Result<SyncReport> {
        schedule::sync_schedules(&self.pool, declared).await
    }

    async fn schedule_tick(&self) -> Result<usize> {
        schedule::tick(&self.pool).await
    }

    // -------- groups -------------------------------------------------------

    async fn group_set_concurrency(&self, key: &str, max: i32) -> Result<()> {
        group::set_concurrency(&self.pool, key, max).await
    }

    async fn group_set_paused(&self, key: &str, paused: bool) -> Result<()> {
        group::set_paused(&self.pool, key, paused).await
    }

    async fn group_get(&self, key: &str) -> Result<Option<Group>> {
        group::get(&self.pool, key).await
    }

    async fn group_list(&self) -> Result<Vec<Group>> {
        group::list(&self.pool).await
    }

    async fn group_set_rate(&self, key: &str, count: u32, period: Duration) -> Result<()> {
        group::set_rate(&self.pool, key, count, period).await
    }

    async fn group_clear_rate(&self, key: &str) -> Result<()> {
        group::clear_rate(&self.pool, key).await
    }

    async fn group_set_rule(&self, pattern: &str, rule: GroupRule) -> Result<()> {
        group::set_rule(&self.pool, pattern, rule).await
    }

    async fn group_remove_rule(&self, pattern: &str) -> Result<bool> {
        group::remove_rule(&self.pool, pattern).await
    }

    async fn group_list_rules(&self) -> Result<Vec<StoredRule>> {
        group::list_rules(&self.pool).await
    }

    // -------- named queues -------------------------------------------------

    async fn queue_set_concurrency(&self, name: &str, max: i32) -> Result<()> {
        named_queue::set_concurrency(&self.pool, name, max).await
    }

    async fn queue_set_paused(&self, name: &str, paused: bool) -> Result<()> {
        named_queue::set_paused(&self.pool, name, paused).await
    }

    async fn queue_get(&self, name: &str) -> Result<Option<NamedQueue>> {
        named_queue::get(&self.pool, name).await
    }

    async fn queue_list(&self) -> Result<Vec<NamedQueue>> {
        named_queue::list(&self.pool).await
    }

    async fn queue_set_timeout(&self, name: &str, timeout: Option<Duration>) -> Result<()> {
        named_queue::set_timeout(&self.pool, name, timeout).await
    }

    // -------- read-only ----------------------------------------------------

    async fn get_stats(&self) -> Result<JobStats> {
        stats::get_stats(&self.pool).await
    }

    async fn list_jobs(&self, filter: ListJobsFilter, pagination: Pagination) -> Result<JobList> {
        stats::list_jobs(&self.pool, filter, pagination).await
    }

    async fn cancel(&self, id: JobId) -> Result<bool> {
        fetch::cancel(&self.pool, id).await
    }
}
