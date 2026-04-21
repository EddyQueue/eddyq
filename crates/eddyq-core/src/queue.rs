use std::{sync::Arc, time::Duration};

use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::{
    enqueue::{EnqueueOptions, EnqueueResult, enqueue},
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
    /// When `true`, do not spawn a LISTEN/NOTIFY listener. Use this when connected
    /// through PgBouncer in transaction-pooling mode (LISTEN is incompatible).
    pub poll_only: bool,
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
            poll_only: false,
        }
    }
}

pub struct QueueBuilder {
    pool: PgPool,
    registry: WorkerRegistry,
    config: QueueConfig,
    line: String,
}

impl QueueBuilder {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            registry: WorkerRegistry::new(),
            config: QueueConfig::default(),
            line: crate::migrate::DEFAULT_LINE.to_owned(),
        }
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

    pub fn build(self) -> Queue {
        Queue {
            pool: self.pool,
            registry: Arc::new(self.registry),
            config: self.config,
            line: self.line,
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

pub struct Queue {
    pool: PgPool,
    registry: Arc<WorkerRegistry>,
    config: QueueConfig,
    line: String,
    state: std::sync::Mutex<QueueState>,
}

impl Queue {
    pub fn builder(pool: PgPool) -> QueueBuilder {
        QueueBuilder::new(pool)
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// The migration line this queue was built for (default: `"main"`).
    pub fn line(&self) -> &str {
        &self.line
    }

    /// Apply all pending schema migrations for this queue's line. Uses the
    /// `_eddyq_migrations` tracking table (intentionally separate from your
    /// app's migration tool so there are no collisions).
    pub async fn migrate(&self) -> Result<crate::migrate::MigrateReport> {
        crate::migrate::up(&self.pool, &self.line).await
    }

    pub async fn migrate_down(&self, max_steps: usize) -> Result<crate::migrate::MigrateReport> {
        crate::migrate::down(&self.pool, &self.line, max_steps).await
    }

    pub async fn migration_status(&self) -> Result<Vec<crate::migrate::MigrationStatus>> {
        crate::migrate::status(&self.pool, &self.line).await
    }

    pub async fn enqueue<J: Job>(&self, job: &J) -> Result<EnqueueResult> {
        enqueue(&self.pool, job, EnqueueOptions::default()).await
    }

    pub async fn enqueue_with<J: Job>(
        &self,
        job: &J,
        opts: EnqueueOptions,
    ) -> Result<EnqueueResult> {
        enqueue(&self.pool, job, opts).await
    }

    /// Enqueue a job inside the caller's transaction. The job row is only
    /// visible to workers if the user's transaction commits. On rollback the
    /// job — and any follow-on NOTIFY — are discarded.
    pub async fn enqueue_in_tx<J: Job>(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        job: &J,
    ) -> Result<EnqueueResult> {
        crate::enqueue::enqueue_in_tx(tx, job, EnqueueOptions::default()).await
    }

    pub async fn enqueue_in_tx_with<J: Job>(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        job: &J,
        opts: EnqueueOptions,
    ) -> Result<EnqueueResult> {
        crate::enqueue::enqueue_in_tx(tx, job, opts).await
    }

    /// Register or update a recurring schedule. Jobs will be auto-enqueued when
    /// each cron occurrence is due. Skip-missed semantics: one enqueue per tick,
    /// regardless of how many runs were missed while the scheduler was down.
    pub async fn add_schedule<J: Job>(
        &self,
        name: &str,
        cron_expr: &str,
        job: &J,
    ) -> Result<()> {
        crate::schedule::upsert_schedule(&self.pool, name, cron_expr, job).await
    }

    pub async fn remove_schedule(&self, name: &str) -> Result<bool> {
        crate::schedule::remove_schedule(&self.pool, name).await
    }

    pub async fn set_schedule_enabled(&self, name: &str, enabled: bool) -> Result<bool> {
        crate::schedule::set_enabled(&self.pool, name, enabled).await
    }

    pub async fn list_schedules(&self) -> Result<Vec<crate::schedule::Schedule>> {
        crate::schedule::list_schedules(&self.pool).await
    }

    /// Set the concurrency cap for a group. Jobs with `group_key(key)` will not
    /// run more than `max` at a time.
    pub async fn set_group_concurrency(&self, key: &str, max: i32) -> Result<()> {
        crate::group::set_concurrency(&self.pool, key, max).await
    }

    pub async fn pause_group(&self, key: &str) -> Result<()> {
        crate::group::set_paused(&self.pool, key, true).await
    }

    pub async fn resume_group(&self, key: &str) -> Result<()> {
        crate::group::set_paused(&self.pool, key, false).await
    }

    pub async fn get_group(&self, key: &str) -> Result<Option<crate::group::Group>> {
        crate::group::get(&self.pool, key).await
    }

    pub async fn list_groups(&self) -> Result<Vec<crate::group::Group>> {
        crate::group::list(&self.pool).await
    }

    /// Set a throughput rate limit: at most `count` jobs may *start* per `period`
    /// for this group. Independent of `max_concurrency` — both constraints apply.
    /// Useful for external-API rate limits (e.g. 1000 req/min for OpenAI).
    pub async fn set_group_rate(
        &self,
        key: &str,
        count: u32,
        period: Duration,
    ) -> Result<()> {
        crate::group::set_rate(&self.pool, key, count, period).await
    }

    pub async fn clear_group_rate(&self, key: &str) -> Result<()> {
        crate::group::clear_rate(&self.pool, key).await
    }

    // --- Pattern-based group rules -----------------------------------------

    /// Register a default-values rule for a group-key glob pattern. Any group
    /// whose key matches this pattern will be auto-configured with these
    /// defaults on its first `enqueue()`, unless you've already explicitly
    /// called `set_group_concurrency` / `set_group_rate` for that specific key.
    ///
    /// Patterns use `*` (any chars) and `?` (one char).
    ///
    /// ```ignore
    /// // Every Shopify integration auto-caps at 2 concurrent workers:
    /// queue.set_group_rule("shopify:*", GroupRule::concurrency(2)).await?;
    ///
    /// // OpenAI calls per tenant: 2000 requests/min, cap 20 in flight:
    /// queue.set_group_rule(
    ///     "tenant:*:openai",
    ///     GroupRule::both(20, 2000, Duration::from_secs(60)),
    /// ).await?;
    /// ```
    pub async fn set_group_rule(
        &self,
        pattern: &str,
        rule: crate::group::GroupRule,
    ) -> Result<()> {
        crate::group::set_rule(&self.pool, pattern, rule).await
    }

    pub async fn remove_group_rule(&self, pattern: &str) -> Result<bool> {
        crate::group::remove_rule(&self.pool, pattern).await
    }

    pub async fn list_group_rules(&self) -> Result<Vec<crate::group::StoredRule>> {
        crate::group::list_rules(&self.pool).await
    }

    pub fn start(&self) -> Result<()> {
        let mut state = self.state.lock().expect("queue state lock poisoned");
        if matches!(*state, QueueState::Running { .. }) {
            return Err(Error::AlreadyRunning);
        }

        let shutdown = CancellationToken::new();
        let handles = runtime::start(
            self.pool.clone(),
            self.registry.clone(),
            self.config.clone(),
            shutdown.clone(),
        );

        info!(
            kinds = ?self.registry.kinds(),
            concurrency = self.config.worker_concurrency,
            "eddyq queue started"
        );

        *state = QueueState::Running { shutdown, handles };
        Ok(())
    }

    pub async fn shutdown(&self) -> Result<()> {
        let (shutdown, handles) = {
            let mut state = self.state.lock().expect("queue state lock poisoned");
            match std::mem::replace(&mut *state, QueueState::Idle) {
                QueueState::Idle => return Err(Error::NotRunning),
                QueueState::Running { shutdown, handles } => (shutdown, handles),
            }
        };

        shutdown.cancel();
        crate::runtime::await_all(handles).await;

        info!("eddyq queue stopped");
        Ok(())
    }
}
