//! Per-system adapters. Each runner exposes the same `QueueUnderTest` trait so scenarios
//! can be run against every backend with identical load patterns.

pub mod bullmq;
pub mod eddyq;
pub mod graphile;
pub mod sqlxmq;

use async_trait::async_trait;

/// A queue-like system under test.
///
/// Implementations wrap an external system (Postgres-backed or Redis-backed), expose
/// enqueue and worker primitives, and report back timing / success metrics.
#[async_trait]
pub trait QueueUnderTest: Send + Sync {
    /// Name shown in the bench report (e.g. `"eddyq"`, `"sqlxmq"`).
    fn name(&self) -> &'static str;

    /// Prepare the system (run migrations, flush Redis, etc.).
    async fn prepare(&self) -> anyhow::Result<()>;

    /// Enqueue a single job. The runner owns the payload shape.
    async fn enqueue(&self, payload: serde_json::Value) -> anyhow::Result<()>;

    /// Start workers; block until `n_jobs` have been processed or `timeout` elapses.
    async fn run_workers(
        &self,
        n_workers: usize,
        n_jobs: usize,
        timeout: std::time::Duration,
    ) -> anyhow::Result<RunStats>;

    /// Tear down workers and clean up.
    async fn teardown(&self) -> anyhow::Result<()>;
}

/// Per-run timing and success statistics.
#[derive(Debug, Clone)]
pub struct RunStats {
    pub jobs_completed: usize,
    pub jobs_failed: usize,
    pub elapsed: std::time::Duration,
    pub p50_latency: std::time::Duration,
    pub p99_latency: std::time::Duration,
    pub p999_latency: std::time::Duration,
    pub peak_memory_bytes: u64,
}
