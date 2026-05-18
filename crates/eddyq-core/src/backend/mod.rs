//! Pluggable backend abstraction.
//!
//! `Queue<B: Backend>` is generic on its backend, so backend-specific
//! capabilities (e.g. `PgBackend::enqueue_in_tx`) live as inherent methods
//! on the concrete struct and a Redis queue can never call them — compile
//! error, not runtime error.
//!
//! The trait is also object-safe (`Arc<dyn Backend>`) so the runtime can
//! take a trait object internally; that lets future tooling (CLI, dashboard)
//! operate on a `Box<dyn Backend>` without knowing the concrete type.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    enqueue::{BulkEnqueueResult, DynEnqueue, EnqueueResult},
    error::Result,
    fetch::{ClaimedJob, Retention},
    group::{Group, GroupRule, StoredRule},
    job::JobId,
    named_queue::NamedQueue,
    schedule::{Schedule, ScheduleDeclaration, SyncReport},
    stats::{JobList, JobStats, ListJobsFilter, Pagination},
};

pub mod pg;

pub use pg::PgBackend;

/// Capability flags advertised by a backend. The `Queue<B>` generic gives us
/// compile-time enforcement of the big differences (`enqueue_in_tx` is a
/// `PgBackend` inherent method). `BackendCaps` is for runtime introspection
/// — dashboards, capability checks before calling something that might
/// surface `Error::Unsupported`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendCaps {
    pub name: &'static str,
    /// Atomic enqueue inside the user's application transaction. Postgres: true.
    pub transactional_enqueue: bool,
    /// Schema migrations apply (vs. structureless KV). Postgres: true.
    pub migrations: bool,
    /// `pg_notify`-style fast wakeup on enqueue. Postgres: true. Redis: true (pubsub).
    pub fast_wakeup: bool,
    /// Soft-cancel of in-flight jobs is supported.
    pub cancel_running: bool,
    /// Inclusive priority range. Postgres `i16`. Redis `(1, 2_097_152)`.
    pub priority_range: (i32, i32),
    /// Safe to run on a Redis Cluster (single-slot per queue via hash-tags).
    pub cluster_safe: bool,
}

/// Job states that `Backend::clean` can target. Restricted to the three
/// finalized states — cleaning `wait`/`active`/`delayed` is out of scope
/// for this surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanState {
    Completed,
    Failed,
    Cancelled,
}

impl CleanState {
    pub fn as_str(self) -> &'static str {
        match self {
            CleanState::Completed => "completed",
            CleanState::Failed => "failed",
            CleanState::Cancelled => "cancelled",
        }
    }
}

#[async_trait]
pub trait Backend: Send + Sync + std::fmt::Debug + 'static {
    fn caps(&self) -> BackendCaps;

    // -------- enqueue ------------------------------------------------------

    async fn enqueue(&self, req: DynEnqueue) -> Result<EnqueueResult>;
    async fn enqueue_many(&self, reqs: Vec<DynEnqueue>) -> Result<BulkEnqueueResult>;

    // -------- worker runtime hot path -------------------------------------

    async fn claim_batch(
        &self,
        worker_id: Uuid,
        batch_size: usize,
        kinds: &[String],
        queues: &[String],
    ) -> Result<Vec<ClaimedJob>>;

    async fn update_heartbeat_batch(&self, ids: &[JobId]) -> Result<u64>;

    async fn mark_completed(
        &self,
        id: JobId,
        worker_id: Uuid,
        result: Option<serde_json::Value>,
    ) -> Result<()>;

    async fn mark_failed(
        &self,
        id: JobId,
        worker_id: Uuid,
        error_entry: serde_json::Value,
        retry_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<()>;

    async fn sweep_stale(&self, stale_after: Duration) -> Result<u64>;

    /// Returns `(completed, failed, cancelled, batches)` deleted counts.
    async fn cleanup(&self, retention: Retention) -> Result<(u64, u64, u64, u64)>;

    /// Ad-hoc retention sweep. Deletes up to `limit` finalized jobs of
    /// the given state that are older than `grace`. Used for one-shot
    /// pruning from admin/API surfaces. Returns the number of rows
    /// actually deleted.
    async fn clean(&self, grace: Duration, limit: u32, state: CleanState) -> Result<u64>;

    /// Proactively reclaim rows this pod claimed but won't get to finish —
    /// used by `Queue::shutdown_with(ShutdownMode::Force)`. Returns the
    /// number of rows actually moved back to `pending`.
    async fn reclaim_in_flight(&self, ids: &[JobId]) -> Result<u64>;

    /// Optional listener task that pings `wakeup` whenever fresh work might
    /// be available. Postgres uses LISTEN/NOTIFY; Redis uses pubsub.
    /// Returning `None` is fine — the fetcher falls back to polling on the
    /// configured interval.
    fn spawn_wakeup_listener(
        self: Arc<Self>,
        wakeup: Arc<Notify>,
        shutdown: CancellationToken,
    ) -> Option<JoinHandle<()>>;

    // -------- leader election ---------------------------------------------

    async fn leader_try_elect(&self, worker_id: Uuid, role: &str, lease_secs: u64) -> Result<bool>;
    async fn leader_resign(&self, worker_id: Uuid, role: &str) -> Result<()>;

    /// Optional fast-wakeup listener for peer-leader resignations. Pings
    /// `on_resign` so a non-leader fires an immediate election instead of
    /// waiting for the next refresh tick.
    fn spawn_leader_resign_listener(
        self: Arc<Self>,
        on_resign: Arc<Notify>,
        shutdown: CancellationToken,
    ) -> Option<JoinHandle<()>>;

    // -------- schedules ----------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    async fn upsert_schedule_raw(
        &self,
        name: &str,
        cron_expr: &str,
        kind: &str,
        payload: serde_json::Value,
        priority: i16,
        max_attempts: i32,
        queue: &str,
    ) -> Result<()>;
    /// Upsert an interval-driven schedule (`{ every: ms }`). Fires every
    /// `interval_ms`; skip-missed semantics match the cron path.
    /// Default implementation returns `Unsupported` — backends that don't
    /// expose intervals can leave it unimplemented.
    #[allow(clippy::too_many_arguments)]
    async fn upsert_interval_schedule_raw(
        &self,
        _name: &str,
        _interval_ms: i64,
        _kind: &str,
        _payload: serde_json::Value,
        _priority: i16,
        _max_attempts: i32,
        _queue: &str,
    ) -> Result<()> {
        Err(crate::error::Error::Unsupported(
            "upsert_interval_schedule_raw not supported by this backend".into(),
        ))
    }
    async fn remove_schedule(&self, name: &str) -> Result<bool>;
    async fn set_schedule_enabled(&self, name: &str, enabled: bool) -> Result<bool>;
    async fn list_schedules(&self) -> Result<Vec<Schedule>>;
    async fn sync_schedules(&self, declared: &[ScheduleDeclaration]) -> Result<SyncReport>;
    async fn schedule_tick(&self) -> Result<usize>;

    // -------- groups -------------------------------------------------------

    async fn group_set_concurrency(&self, key: &str, max: i32) -> Result<()>;
    async fn group_set_paused(&self, key: &str, paused: bool) -> Result<()>;
    async fn group_get(&self, key: &str) -> Result<Option<Group>>;
    async fn group_list(&self) -> Result<Vec<Group>>;
    async fn group_set_rate(&self, key: &str, count: u32, period: Duration) -> Result<()>;
    async fn group_clear_rate(&self, key: &str) -> Result<()>;
    async fn group_set_rule(&self, pattern: &str, rule: GroupRule) -> Result<()>;
    async fn group_remove_rule(&self, pattern: &str) -> Result<bool>;
    async fn group_list_rules(&self) -> Result<Vec<StoredRule>>;

    // -------- named queues -------------------------------------------------

    async fn queue_set_concurrency(&self, name: &str, max: i32) -> Result<()>;
    async fn queue_set_paused(&self, name: &str, paused: bool) -> Result<()>;
    async fn queue_get(&self, name: &str) -> Result<Option<NamedQueue>>;
    async fn queue_list(&self) -> Result<Vec<NamedQueue>>;
    async fn queue_set_timeout(&self, name: &str, timeout: Option<Duration>) -> Result<()>;

    // -------- read-only ----------------------------------------------------

    async fn get_stats(&self) -> Result<JobStats>;
    async fn list_jobs(&self, filter: ListJobsFilter, pagination: Pagination) -> Result<JobList>;
    async fn cancel(&self, id: JobId) -> Result<bool>;

    // Migrations are NOT on the trait. They're a SQL-specific concern and
    // live as inherent methods on `PgBackend`. Generic admin tooling
    // checks `caps().migrations` first.
}
