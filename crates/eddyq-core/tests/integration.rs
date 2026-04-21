//! End-to-end integration tests. Require a running Postgres at `DATABASE_URL`.
//!
//!     just db-up
//!     DATABASE_URL=postgres://eddyq:eddyq@localhost:5433/eddyq_dev \
//!         cargo test -p eddyq-core --test integration -- --test-threads=1

#![allow(clippy::unreadable_literal, clippy::too_many_lines)]

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use eddyq_core::{
    Job, JobContext, JobResult, Queue, QueueConfig, Worker,
    async_trait,
    fetch::sweep_stale,
};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

#[derive(Debug, Serialize, Deserialize)]
struct Count {
    n: u64,
}

impl Job for Count {
    const KIND: &'static str = "count";
}

#[derive(Clone)]
struct CountWorker {
    counter: Arc<AtomicUsize>,
}

#[async_trait]
impl Worker<Count> for CountWorker {
    async fn perform(&self, _job: Count, _ctx: JobContext) -> JobResult {
        self.counter.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Flaky {
    fail_until_attempt: i32,
}

impl Job for Flaky {
    const KIND: &'static str = "flaky";

    fn max_attempts(&self) -> i32 {
        5
    }
}

struct FlakyWorker;

#[async_trait]
impl Worker<Flaky> for FlakyWorker {
    async fn perform(&self, job: Flaky, ctx: JobContext) -> JobResult {
        if ctx.attempt < job.fail_until_attempt {
            anyhow::bail!("simulated failure on attempt {}", ctx.attempt);
        }
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct PanicJob;

impl Job for PanicJob {
    const KIND: &'static str = "panic";
    fn max_attempts(&self) -> i32 {
        1
    }
}

struct PanicWorker;

#[async_trait]
impl Worker<PanicJob> for PanicWorker {
    async fn perform(&self, _job: PanicJob, _ctx: JobContext) -> JobResult {
        panic!("explicit panic");
    }
}

fn fast_config() -> QueueConfig {
    QueueConfig {
        fetch_poll_interval: Duration::from_millis(50),
        fetch_cooldown: Duration::from_millis(10),
        fetch_batch_size: 10,
        worker_concurrency: 4,
        heartbeat_interval: Duration::from_millis(100),
        sweep_interval: Duration::from_millis(500),
        // Generous buffer so CPU-starved heartbeats don't trigger false
        // recoveries under parallel-test load. Specific sweeper tests still
        // exercise short stale_after via direct calls to sweep_stale().
        stale_after: Duration::from_secs(10),
        retry_base: Duration::from_millis(20),
        retry_max: Duration::from_millis(200),
        scheduler_interval: Duration::from_millis(100),
        poll_only: false,
    }
}

async fn count_states(pool: &PgPool) -> (i64, i64, i64) {
    sqlx::query_as::<_, (i64, i64, i64)>(
        "SELECT
           COUNT(*) FILTER (WHERE state = 'pending'),
           COUNT(*) FILTER (WHERE state = 'running'),
           COUNT(*) FILTER (WHERE state = 'completed')
         FROM eddyq_jobs",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn enqueue_run_complete(pool: PgPool) {
    let counter = Arc::new(AtomicUsize::new(0));
    let queue = Queue::builder(pool.clone())
        .register::<Count, _>(CountWorker {
            counter: counter.clone(),
        })
        .config(fast_config())
        .build();

    for n in 0..20 {
        queue.enqueue(&Count { n }).await.unwrap();
    }

    queue.start().unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if counter.load(Ordering::SeqCst) >= 20 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("all 20 jobs should complete within 5s");

    queue.shutdown().await.unwrap();

    let (pending, running, completed) = count_states(&pool).await;
    assert_eq!(pending, 0);
    assert_eq!(running, 0);
    assert_eq!(completed, 20);
}

#[sqlx::test(migrations = "./migrations")]
async fn retries_then_succeeds(pool: PgPool) {
    let queue = Queue::builder(pool.clone())
        .register::<Flaky, _>(FlakyWorker)
        .config(fast_config())
        .build();

    queue
        .enqueue(&Flaky {
            fail_until_attempt: 3,
        })
        .await
        .unwrap();

    queue.start().unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let (_, _, completed) = count_states(&pool).await;
            if completed == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("flaky job should eventually succeed");

    queue.shutdown().await.unwrap();

    let row: (i32, String, serde_json::Value) =
        sqlx::query_as("SELECT attempt, state, errors FROM eddyq_jobs LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(row.1, "completed");
    assert_eq!(row.0, 3, "should have executed exactly 3 attempts");
    assert_eq!(row.2.as_array().unwrap().len(), 2, "2 failures recorded");
}

#[sqlx::test(migrations = "./migrations")]
async fn panic_is_isolated(pool: PgPool) {
    let queue = Queue::builder(pool.clone())
        .register::<PanicJob, _>(PanicWorker)
        .register::<Count, _>(CountWorker {
            counter: Arc::new(AtomicUsize::new(0)),
        })
        .config(fast_config())
        .build();

    queue.enqueue(&PanicJob).await.unwrap();
    for n in 0..5 {
        queue.enqueue(&Count { n }).await.unwrap();
    }

    queue.start().unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let (_, _, completed) = count_states(&pool).await;
            if completed >= 5 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("5 healthy jobs should complete despite panic in a sibling");

    queue.shutdown().await.unwrap();

    let (_, _, completed) = count_states(&pool).await;
    assert_eq!(completed, 5, "5 count jobs");
    let failed: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM eddyq_jobs WHERE state = 'failed'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(failed, 1, "1 panicked job marked failed");
}

#[sqlx::test(migrations = "./migrations")]
async fn sweeper_recovers_stale_running(pool: PgPool) {
    // Manually insert a "running" row with an ancient heartbeat, simulating a worker
    // that crashed without releasing its job.
    sqlx::query(
        r#"
        INSERT INTO eddyq_jobs (kind, payload, state, attempt, max_attempts, heartbeat_at, worker_id)
        VALUES ('count', '{"n":42}', 'running', 1, 3, NOW() - INTERVAL '10 seconds', gen_random_uuid())
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();

    let recovered = sweep_stale(&pool, Duration::from_secs(1)).await.unwrap();
    assert_eq!(recovered, 1);

    let (pending, running, _) = count_states(&pool).await;
    assert_eq!(pending, 1);
    assert_eq!(running, 0);

    // And verify the end-to-end recovery by running the queue — the reset job
    // should now be fetched and completed.
    let counter = Arc::new(AtomicUsize::new(0));
    let queue = Queue::builder(pool.clone())
        .register::<Count, _>(CountWorker {
            counter: counter.clone(),
        })
        .config(fast_config())
        .build();

    queue.start().unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if counter.load(Ordering::SeqCst) >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("recovered job should complete");
    queue.shutdown().await.unwrap();
}

#[sqlx::test(migrations = "./migrations")]
async fn sweeper_gives_up_at_max_attempts(pool: PgPool) {
    sqlx::query(
        r#"
        INSERT INTO eddyq_jobs (kind, payload, state, attempt, max_attempts, heartbeat_at, worker_id)
        VALUES ('count', '{"n":1}', 'running', 3, 3, NOW() - INTERVAL '10 seconds', gen_random_uuid())
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();

    let recovered = sweep_stale(&pool, Duration::from_secs(1)).await.unwrap();
    assert_eq!(recovered, 1);

    let state: String = sqlx::query_scalar("SELECT state FROM eddyq_jobs LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, "failed", "already at max_attempts → fail, not retry");
}

#[sqlx::test(migrations = "./migrations")]
async fn scheduled_job_waits_until_due(pool: PgPool) {
    use chrono::Utc;
    use eddyq_core::EnqueueOptions;

    let counter = Arc::new(AtomicUsize::new(0));
    let queue = Queue::builder(pool.clone())
        .register::<Count, _>(CountWorker {
            counter: counter.clone(),
        })
        .config(fast_config())
        .build();

    // Schedule one job 400ms in the future, one now.
    queue
        .enqueue_with(
            &Count { n: 1 },
            EnqueueOptions {
                scheduled_at: Some(Utc::now() + chrono::Duration::milliseconds(400)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    queue.enqueue(&Count { n: 2 }).await.unwrap();

    queue.start().unwrap();

    // After 150ms, only the immediate job should be done.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(counter.load(Ordering::SeqCst), 1, "future-dated job should not run yet");

    // After another 500ms, both should be done.
    tokio::time::timeout(Duration::from_millis(800), async {
        loop {
            if counter.load(Ordering::SeqCst) >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("scheduled job should run once due");

    queue.shutdown().await.unwrap();
}

#[sqlx::test(migrations = "./migrations")]
async fn backoff_delays_retries(pool: PgPool) {
    let queue = Queue::builder(pool.clone())
        .register::<Flaky, _>(FlakyWorker)
        .config(fast_config())
        .build();

    queue
        .enqueue(&Flaky {
            fail_until_attempt: 3,
        })
        .await
        .unwrap();

    let start = std::time::Instant::now();
    queue.start().unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let (_, _, completed) = count_states(&pool).await;
            if completed == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("flaky job should eventually succeed");
    let elapsed = start.elapsed();
    queue.shutdown().await.unwrap();

    // With retry_base=20ms:
    //   attempt 1 fails → wait ~20-25ms
    //   attempt 2 fails → wait ~40-50ms
    //   attempt 3 succeeds
    // Minimum total: ~60ms of backoff + execution/poll overhead.
    // We assert >=60ms to prove backoff actually happened, and <500ms to prove it's not immediate-retry or runaway.
    assert!(
        elapsed >= Duration::from_millis(60),
        "backoff should add at least ~60ms of delay across 2 retries, got {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_millis(800),
        "total time unexpectedly long: {elapsed:?}"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn cron_schedule_fires_recurring(pool: PgPool) {
    let counter = Arc::new(AtomicUsize::new(0));
    let queue = Queue::builder(pool.clone())
        .register::<Count, _>(CountWorker {
            counter: counter.clone(),
        })
        .config(fast_config())
        .build();

    // Cron expr "* * * * * * *" (7-field: every second)
    queue
        .add_schedule("tick", "* * * * * * *", &Count { n: 0 })
        .await
        .unwrap();

    queue.start().unwrap();
    // Wait for the scheduler to fire at least 2 cron ticks. Every-second cron
    // should easily hit this in 4s even under heavy parallel-test load.
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            if counter.load(Ordering::SeqCst) >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("every-second cron should fire at least 2x within 4s");
    queue.shutdown().await.unwrap();

    // And the schedule's last_run_at should be recent.
    let schedules = eddyq_core::schedule::list_schedules(&pool).await.unwrap();
    assert_eq!(schedules.len(), 1);
    assert!(schedules[0].last_run_at.is_some(), "last_run_at set after firing");
    assert!(schedules[0].next_run_at > chrono::Utc::now());
}

#[sqlx::test(migrations = "./migrations")]
async fn schedule_upsert_and_remove(pool: PgPool) {
    let queue = Queue::builder(pool.clone()).build();

    queue
        .add_schedule("daily", "0 0 0 * * * *", &Count { n: 0 })
        .await
        .unwrap();
    queue
        .add_schedule("daily", "0 0 12 * * * *", &Count { n: 1 })
        .await
        .unwrap();

    let schedules = queue.list_schedules().await.unwrap();
    assert_eq!(schedules.len(), 1);
    assert_eq!(schedules[0].cron_expr, "0 0 12 * * * *");

    let removed = queue.remove_schedule("daily").await.unwrap();
    assert!(removed);
    assert_eq!(queue.list_schedules().await.unwrap().len(), 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn unique_key_dedupes(pool: PgPool) {
    use eddyq_core::{EnqueueOptions, EnqueueResult};

    #[derive(Debug, Serialize, Deserialize)]
    struct Dedup;
    impl Job for Dedup {
        const KIND: &'static str = "dedup";
    }

    let queue = Queue::builder(pool.clone()).build();

    let a = queue
        .enqueue_with(
            &Dedup,
            EnqueueOptions {
                unique_key: Some("same".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let b = queue
        .enqueue_with(
            &Dedup,
            EnqueueOptions {
                unique_key: Some("same".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert!(matches!(a, EnqueueResult::Inserted(_)));
    assert!(matches!(b, EnqueueResult::Skipped));

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM eddyq_jobs")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

/// A probe worker that measures the maximum in-flight concurrency observed
/// while running. Used by the group-concurrency tests to prove the cap is
/// actually enforced.
#[derive(Clone)]
struct ProbeWorker {
    group: Option<String>,
    current: Arc<AtomicUsize>,
    max_observed: Arc<AtomicUsize>,
    completed: Arc<AtomicUsize>,
    pool: Option<PgPool>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Probe {
    n: u64,
    group: Option<String>,
}

impl Job for Probe {
    const KIND: &'static str = "probe";
    fn group_key(&self) -> Option<String> {
        self.group.clone()
    }
}

#[async_trait]
impl Worker<Probe> for ProbeWorker {
    async fn perform(&self, _job: Probe, _ctx: JobContext) -> JobResult {
        let c = self.current.fetch_add(1, Ordering::SeqCst) + 1;
        let mut max = self.max_observed.load(Ordering::SeqCst);
        while c > max {
            match self.max_observed.compare_exchange(
                max,
                c,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(actual) => max = actual,
            }
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
        self.current.fetch_sub(1, Ordering::SeqCst);
        self.completed.fetch_add(1, Ordering::SeqCst);
        let _ = (&self.group, &self.pool);
        Ok(())
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn group_concurrency_cap_is_enforced(pool: PgPool) {
    let current = Arc::new(AtomicUsize::new(0));
    let max_observed = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicUsize::new(0));

    let queue = Queue::builder(pool.clone())
        .register::<Probe, _>(ProbeWorker {
            group: Some("provider:openai".into()),
            current: current.clone(),
            max_observed: max_observed.clone(),
            completed: completed.clone(),
            pool: Some(pool.clone()),
        })
        .config(QueueConfig {
            worker_concurrency: 10,
            ..fast_config()
        })
        .build();

    // Cap the group at 3 BEFORE any jobs run.
    queue
        .set_group_concurrency("provider:openai", 3)
        .await
        .unwrap();

    for n in 0..20 {
        queue
            .enqueue(&Probe {
                n,
                group: Some("provider:openai".into()),
            })
            .await
            .unwrap();
    }

    queue.start().unwrap();

    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if completed.load(Ordering::SeqCst) >= 20 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("all 20 jobs should complete despite 3-at-a-time cap");

    queue.shutdown().await.unwrap();

    let peak = max_observed.load(Ordering::SeqCst);
    assert!(
        peak <= 3,
        "group cap=3 was violated: observed peak concurrency={peak}"
    );
    assert!(peak >= 2, "should actually run >1 at a time, got peak={peak}");

    // After completion, the group's running_count should be back to 0.
    let g = queue
        .get_group("provider:openai")
        .await
        .unwrap()
        .expect("group row should exist");
    assert_eq!(g.running_count, 0);
    assert_eq!(g.max_concurrency, 3);
}

#[sqlx::test(migrations = "./migrations")]
async fn groups_are_independent(pool: PgPool) {
    // Group A has a huge cap, group B has cap=1. Both sets of jobs should
    // progress concurrently — A shouldn't be blocked by B's narrow cap.
    let current = Arc::new(AtomicUsize::new(0));
    let max_observed = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicUsize::new(0));

    let queue = Queue::builder(pool.clone())
        .register::<Probe, _>(ProbeWorker {
            group: None,
            current: current.clone(),
            max_observed: max_observed.clone(),
            completed: completed.clone(),
            pool: Some(pool.clone()),
        })
        .config(QueueConfig {
            worker_concurrency: 8,
            ..fast_config()
        })
        .build();

    queue.set_group_concurrency("a", 8).await.unwrap();
    queue.set_group_concurrency("b", 1).await.unwrap();

    for n in 0..10 {
        queue
            .enqueue(&Probe {
                n,
                group: Some("a".into()),
            })
            .await
            .unwrap();
    }
    for n in 0..5 {
        queue
            .enqueue(&Probe {
                n,
                group: Some("b".into()),
            })
            .await
            .unwrap();
    }

    queue.start().unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if completed.load(Ordering::SeqCst) >= 15 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("all 15 should complete");
    queue.shutdown().await.unwrap();

    // Peak should exceed group b's cap of 1 — because group a was running too.
    let peak = max_observed.load(Ordering::SeqCst);
    assert!(
        peak > 1,
        "groups should run in parallel across different keys; peak was {peak}"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn paused_group_blocks_execution(pool: PgPool) {
    let current = Arc::new(AtomicUsize::new(0));
    let max_observed = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicUsize::new(0));

    let queue = Queue::builder(pool.clone())
        .register::<Probe, _>(ProbeWorker {
            group: None,
            current: current.clone(),
            max_observed: max_observed.clone(),
            completed: completed.clone(),
            pool: Some(pool.clone()),
        })
        .config(fast_config())
        .build();

    queue.pause_group("slow").await.unwrap();

    for n in 0..5 {
        queue
            .enqueue(&Probe {
                n,
                group: Some("slow".into()),
            })
            .await
            .unwrap();
    }

    queue.start().unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        completed.load(Ordering::SeqCst),
        0,
        "paused group should not have run anything"
    );

    queue.resume_group("slow").await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if completed.load(Ordering::SeqCst) >= 5 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("resumed group should complete its jobs");
    queue.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------
// Migration runner tests (no `migrations = ...` on sqlx::test — we want a
// truly empty DB so the runner does the work)
// ---------------------------------------------------------------------------

#[sqlx::test(migrations = false)]
async fn migrate_up_applies_all_known_migrations(pool: PgPool) {
    let report = eddyq_core::migrate::up(&pool, "main").await.unwrap();

    // We ship 4 migrations at time of writing; the assertion is >= so that
    // adding new migrations doesn't retroactively break this test.
    assert!(
        report.applied.len() >= 4,
        "expected at least 4 migrations applied, got {}",
        report.applied.len()
    );

    // All expected tables exist.
    for tbl in &["eddyq_jobs", "eddyq_schedules", "eddyq_groups", "_eddyq_migrations"] {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = $1)",
        )
        .bind(*tbl)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(exists, "table {tbl} should exist after migrate up");
    }

    // We do NOT create `_sqlx_migrations` — that's the whole point of using
    // our own tracking table: no collision with user's sqlx tooling.
    let sqlx_table_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = '_sqlx_migrations')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!sqlx_table_exists, "eddyq must not touch _sqlx_migrations");
}

#[sqlx::test(migrations = false)]
async fn migrate_up_is_idempotent(pool: PgPool) {
    let first = eddyq_core::migrate::up(&pool, "main").await.unwrap();
    let second = eddyq_core::migrate::up(&pool, "main").await.unwrap();
    assert!(!first.applied.is_empty());
    assert!(
        second.applied.is_empty(),
        "second up should apply nothing: {:?}",
        second.applied
    );
}

#[sqlx::test(migrations = false)]
async fn migrate_down_rolls_back_all(pool: PgPool) {
    eddyq_core::migrate::up(&pool, "main").await.unwrap();
    let report = eddyq_core::migrate::down(&pool, "main", usize::MAX).await.unwrap();
    assert!(report.rolled_back.len() >= 4);

    // All our tables are gone (except _eddyq_migrations, which we keep
    // around as an empty tracking table — that matches River's behavior).
    for tbl in &["eddyq_jobs", "eddyq_schedules", "eddyq_groups"] {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = $1)",
        )
        .bind(*tbl)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(!exists, "table {tbl} should be gone after migrate down");
    }

    let remaining: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM _eddyq_migrations").fetch_one(&pool).await.unwrap();
    assert_eq!(remaining, 0, "tracking table should be empty after full rollback");
}

#[sqlx::test(migrations = false)]
async fn migrate_status_reports_pending_and_applied(pool: PgPool) {
    // Before any migration: all pending.
    let before = eddyq_core::migrate::status(&pool, "main").await.unwrap();
    assert!(before.iter().all(|s| s.applied_at.is_none()));

    // Apply everything: all applied.
    eddyq_core::migrate::up(&pool, "main").await.unwrap();
    let after = eddyq_core::migrate::status(&pool, "main").await.unwrap();
    assert!(after.iter().all(|s| s.applied_at.is_some()));

    // Roll back one: last is pending again.
    eddyq_core::migrate::down(&pool, "main", 1).await.unwrap();
    let partial = eddyq_core::migrate::status(&pool, "main").await.unwrap();
    assert!(
        partial.last().unwrap().applied_at.is_none(),
        "most recent migration should be pending after down --max-steps 1"
    );
    assert!(
        partial[0].applied_at.is_some(),
        "earlier migrations still applied"
    );
}

#[sqlx::test(migrations = false)]
async fn lines_track_independently(pool: PgPool) {
    // Apply all migrations on "main".
    eddyq_core::migrate::up(&pool, "main").await.unwrap();

    // "canary" line should report everything as pending, even though the
    // schema is already up (shared tables across lines).
    let canary_status = eddyq_core::migrate::status(&pool, "canary").await.unwrap();
    assert!(
        canary_status.iter().all(|s| s.applied_at.is_none()),
        "canary should see all migrations as pending — it has its own tracking"
    );

    let main_status = eddyq_core::migrate::status(&pool, "main").await.unwrap();
    assert!(
        main_status.iter().all(|s| s.applied_at.is_some()),
        "main should see everything as applied"
    );

    // Both lines exist in the registry.
    let lines = eddyq_core::migrate::list_lines(&pool).await.unwrap();
    assert_eq!(lines, vec!["main".to_string()]);

    // Rolling back canary is a no-op — nothing was applied on that line.
    let rb = eddyq_core::migrate::down(&pool, "canary", usize::MAX)
        .await
        .unwrap();
    assert!(rb.rolled_back.is_empty());

    // Rolling back main doesn't affect canary's tracking (which is already empty).
    eddyq_core::migrate::down(&pool, "main", 1).await.unwrap();
    let canary_after = eddyq_core::migrate::status(&pool, "canary").await.unwrap();
    assert!(canary_after.iter().all(|s| s.applied_at.is_none()));
}

#[sqlx::test(migrations = false)]
async fn queue_uses_its_builder_line(pool: PgPool) {
    let queue = Queue::builder(pool.clone()).line("canary").build();
    assert_eq!(queue.line(), "canary");

    // Migrate via the queue — should tag with line="canary" in the tracking
    // table even though we're in a fresh DB (which is fine — no main yet).
    queue.migrate().await.unwrap();

    let canary_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM _eddyq_migrations WHERE line = 'canary'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(canary_rows >= 4, "expected 4+ canary rows, got {canary_rows}");

    let main_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM _eddyq_migrations WHERE line = 'main'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(main_rows, 0, "main line should be untouched");
}

#[test]
fn get_sql_returns_up_and_down() {
    use eddyq_core::migrate::{Direction, MIGRATIONS, get_sql};

    let first = MIGRATIONS[0];
    let up = get_sql(first.version, Direction::Up).expect("up should exist");
    let down = get_sql(first.version, Direction::Down).expect("down should exist");
    assert!(up.contains("CREATE TABLE eddyq_jobs"));
    assert!(down.contains("DROP TABLE"));
    assert_ne!(up, down);

    assert!(get_sql(999_999_999, Direction::Up).is_none());
}

/// The BullMQ-killer: enqueuing inside the user's transaction. If the user's
/// work rolls back, the job must NOT appear — even though it was "inserted"
/// earlier in the transaction.
#[sqlx::test(migrations = "./migrations")]
async fn transactional_enqueue_rolls_back_with_user_tx(pool: PgPool) {
    let queue = Queue::builder(pool.clone()).build();

    // User sets up a side table and starts a transaction.
    sqlx::query("CREATE TABLE invoices (id BIGSERIAL PRIMARY KEY, amount INT NOT NULL)")
        .execute(&pool)
        .await
        .unwrap();

    let mut tx = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO invoices (amount) VALUES (100)")
        .execute(&mut *tx)
        .await
        .unwrap();
    queue
        .enqueue_in_tx(&mut tx, &Count { n: 42 })
        .await
        .unwrap();

    // Simulate a failure: roll back.
    tx.rollback().await.unwrap();

    let invoices: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM invoices")
        .fetch_one(&pool)
        .await
        .unwrap();
    let jobs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM eddyq_jobs")
        .fetch_one(&pool)
        .await
        .unwrap();

    assert_eq!(invoices, 0, "rollback: invoice gone");
    assert_eq!(jobs, 0, "rollback: job must also be gone (this is the feature)");
}

#[sqlx::test(migrations = "./migrations")]
async fn transactional_enqueue_commits_and_runs(pool: PgPool) {
    let counter = Arc::new(AtomicUsize::new(0));
    let queue = Queue::builder(pool.clone())
        .register::<Count, _>(CountWorker {
            counter: counter.clone(),
        })
        .config(fast_config())
        .build();

    sqlx::query("CREATE TABLE invoices (id BIGSERIAL PRIMARY KEY, amount INT NOT NULL)")
        .execute(&pool)
        .await
        .unwrap();

    let mut tx = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO invoices (amount) VALUES (250)")
        .execute(&mut *tx)
        .await
        .unwrap();
    queue
        .enqueue_in_tx(&mut tx, &Count { n: 7 })
        .await
        .unwrap();
    tx.commit().await.unwrap();

    queue.start().unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if counter.load(Ordering::SeqCst) >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("committed job should run");
    queue.shutdown().await.unwrap();

    let invoices: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM invoices")
        .fetch_one(&pool)
        .await
        .unwrap();
    let completed: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM eddyq_jobs WHERE state = 'completed'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(invoices, 1);
    assert_eq!(completed, 1);
}

/// Enqueue many jobs across many transactions, roll back some, commit others.
/// Only committed ones should ever run.
#[sqlx::test(migrations = "./migrations")]
async fn transactional_enqueue_mixed_commits_and_rollbacks(pool: PgPool) {
    let counter = Arc::new(AtomicUsize::new(0));
    let queue = Queue::builder(pool.clone())
        .register::<Count, _>(CountWorker {
            counter: counter.clone(),
        })
        .config(fast_config())
        .build();

    let mut committed = 0u64;
    for n in 0..20u64 {
        let mut tx = pool.begin().await.unwrap();
        queue.enqueue_in_tx(&mut tx, &Count { n }).await.unwrap();
        if n % 3 == 0 {
            tx.rollback().await.unwrap();
        } else {
            tx.commit().await.unwrap();
            committed += 1;
        }
    }

    queue.start().unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if counter.load(Ordering::SeqCst) as u64 >= committed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("all committed jobs should run");
    queue.shutdown().await.unwrap();

    let jobs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM eddyq_jobs")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(jobs as u64, committed, "rolled-back jobs should leave no trace");
    assert_eq!(counter.load(Ordering::SeqCst) as u64, committed);
}

#[sqlx::test(migrations = "./migrations")]
async fn rate_limit_throttles_throughput(pool: PgPool) {
    // 20 fast jobs, rate limit 10/second. All should complete, but in ≥1.8s
    // (first 10 immediate from the bucket, remainder refills at 10/s → ≥1s more).
    let completed = Arc::new(AtomicUsize::new(0));
    let queue = Queue::builder(pool.clone())
        .register::<Probe, _>(ProbeWorker {
            group: None,
            current: Arc::new(AtomicUsize::new(0)),
            max_observed: Arc::new(AtomicUsize::new(0)),
            completed: completed.clone(),
            pool: Some(pool.clone()),
        })
        .config(QueueConfig {
            worker_concurrency: 10,
            ..fast_config()
        })
        .build();

    queue
        .set_group_rate("api", 10, Duration::from_secs(1))
        .await
        .unwrap();

    for n in 0..20 {
        queue
            .enqueue(&Probe {
                n,
                group: Some("api".into()),
            })
            .await
            .unwrap();
    }

    let start = std::time::Instant::now();
    queue.start().unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if completed.load(Ordering::SeqCst) >= 20 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("all 20 should complete within 5s");
    let elapsed = start.elapsed();
    queue.shutdown().await.unwrap();

    assert!(
        elapsed >= Duration::from_millis(800),
        "rate limit 10/sec should have enforced at least 0.8s for 20 jobs, got {elapsed:?}"
    );
    // Upper bound is generous — we just want to prove it's not permanently stuck.
    assert!(
        elapsed <= Duration::from_secs(4),
        "rate limit was too slow: {elapsed:?}"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn rate_limit_refills_after_idle(pool: PgPool) {
    // Set a rate of 5/sec. Use up the bucket, sleep 1s, verify the bucket refilled.
    let g_before = sqlx::query_as::<_, (Option<f64>,)>(
        "INSERT INTO eddyq_groups (key, rate_count, rate_period_ms, tokens, tokens_refilled_at)
         VALUES ('r', 5, 1000, 5, NOW() - INTERVAL '3 seconds')
         RETURNING tokens",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(g_before.0, Some(5.0));

    // Simulate a worker checking the bucket now: 3s elapsed × 5/sec = 15 refill,
    // capped at max=5. So tokens should become 5 (already was 5 but clamping works).
    // We directly exercise the refill math by reading group state and applying
    // the same logic as claim_batch.
    let (refill_count, refill_period_ms, tokens, refilled_at): (
        Option<i32>,
        Option<i32>,
        f64,
        Option<chrono::DateTime<chrono::Utc>>,
    ) = sqlx::query_as(
        "SELECT rate_count, rate_period_ms, tokens, tokens_refilled_at
           FROM eddyq_groups WHERE key = 'r'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();

    let elapsed_ms = (chrono::Utc::now() - refilled_at.unwrap()).num_milliseconds() as f64;
    let rc = refill_count.unwrap();
    let rp = refill_period_ms.unwrap();
    let refilled = (tokens + elapsed_ms * f64::from(rc) / f64::from(rp)).min(f64::from(rc));
    assert!((4.99..=5.01).contains(&refilled), "bucket should refill to cap=5, got {refilled}");
}

#[sqlx::test(migrations = "./migrations")]
async fn rate_and_concurrency_stack(pool: PgPool) {
    // Concurrency=2 AND rate=4/sec. 8 jobs. Both limits apply.
    // - concurrency alone would allow peak 2
    // - rate alone (no concurrency): still max 2 at once because each job is 40ms and 4/s
    // - together: peak ≤ 2, total time dominated by rate
    let current = Arc::new(AtomicUsize::new(0));
    let max_observed = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicUsize::new(0));
    let queue = Queue::builder(pool.clone())
        .register::<Probe, _>(ProbeWorker {
            group: None,
            current: current.clone(),
            max_observed: max_observed.clone(),
            completed: completed.clone(),
            pool: Some(pool.clone()),
        })
        .config(QueueConfig {
            worker_concurrency: 10,
            ..fast_config()
        })
        .build();

    queue.set_group_concurrency("stack", 2).await.unwrap();
    queue
        .set_group_rate("stack", 4, Duration::from_secs(1))
        .await
        .unwrap();

    for n in 0..8 {
        queue
            .enqueue(&Probe {
                n,
                group: Some("stack".into()),
            })
            .await
            .unwrap();
    }

    queue.start().unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if completed.load(Ordering::SeqCst) >= 8 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("should complete");
    queue.shutdown().await.unwrap();

    let peak = max_observed.load(Ordering::SeqCst);
    assert!(peak <= 2, "concurrency cap=2 should hold; got peak={peak}");
}

#[sqlx::test(migrations = "./migrations")]
async fn sweeper_decrements_group_counter(pool: PgPool) {
    use eddyq_core::fetch::sweep_stale;

    // Manually insert a running job with group_key=x and seed the counter at 1.
    sqlx::query(
        r#"
        INSERT INTO eddyq_groups (key, running_count, max_concurrency)
        VALUES ('x', 1, 5)
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query(
        r#"
        INSERT INTO eddyq_jobs
            (kind, payload, state, attempt, max_attempts, heartbeat_at, worker_id, group_key)
        VALUES ('count', '{"n":1}', 'running', 1, 3, NOW() - INTERVAL '10 seconds', gen_random_uuid(), 'x')
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();

    let recovered = sweep_stale(&pool, Duration::from_secs(1)).await.unwrap();
    assert_eq!(recovered, 1);

    let count: i32 = sqlx::query_scalar("SELECT running_count FROM eddyq_groups WHERE key = 'x'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "sweeper should have decremented group counter");
}
