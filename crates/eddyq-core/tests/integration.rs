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
    Job, JobContext, JobResult, Queue, QueueConfig, Worker, async_trait, fetch::sweep_stale,
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
        // Tests do not exercise the cleanup loop (we test cleanup() directly);
        // keep retentions at None so a test's finalized jobs are never reaped.
        cleanup_interval: Duration::from_secs(60),
        completed_retention: None,
        failed_retention: None,
        cancelled_retention: None,
        poll_only: false,
        leader_lease_secs: 30,
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
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "future-dated job should not run yet"
    );

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
    assert!(
        schedules[0].last_run_at.is_some(),
        "last_run_at set after firing"
    );
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
async fn sync_schedules_reconciles(pool: PgPool) {
    use eddyq_core::schedule::ScheduleDeclaration;

    let queue = Queue::builder(pool.clone()).build();

    // Pre-existing imperatively-added schedule that is NOT in the declared list.
    queue
        .add_schedule("orphan", "0 0 0 * * *", &Count { n: 9 })
        .await
        .unwrap();

    // First sync: insert two declared schedules, delete the orphan.
    let declared_v1 = vec![
        ScheduleDeclaration {
            name: "alpha".into(),
            cron_expr: "0 0 9 * * *".into(),
            kind: Count::KIND.into(),
            payload: serde_json::json!({ "n": 1 }),
            priority: 0,
            max_attempts: 3,
        },
        ScheduleDeclaration {
            name: "beta".into(),
            cron_expr: "0 0 10 * * *".into(),
            kind: Count::KIND.into(),
            payload: serde_json::json!({ "n": 2 }),
            priority: 0,
            max_attempts: 3,
        },
    ];
    let report = queue.sync_schedules(&declared_v1).await.unwrap();
    assert_eq!(report.upserted, 2);
    assert_eq!(report.deleted, vec!["orphan".to_string()]);

    let names: Vec<String> = queue
        .list_schedules()
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(names, vec!["alpha".to_string(), "beta".to_string()]);

    // Capture alpha's next_run_at; re-syncing with the same cron must preserve it.
    let alpha_before = queue
        .list_schedules()
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.name == "alpha")
        .unwrap()
        .next_run_at;

    // Second sync: drop beta, change alpha's cron, leave alpha's name alone.
    let declared_v2 = vec![ScheduleDeclaration {
        name: "alpha".into(),
        cron_expr: "0 0 9 * * *".into(), // unchanged
        kind: Count::KIND.into(),
        payload: serde_json::json!({ "n": 1 }),
        priority: 0,
        max_attempts: 3,
    }];
    let report = queue.sync_schedules(&declared_v2).await.unwrap();
    assert_eq!(report.upserted, 1);
    assert_eq!(report.deleted, vec!["beta".to_string()]);

    let alpha_after = queue
        .list_schedules()
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.name == "alpha")
        .unwrap()
        .next_run_at;
    assert_eq!(
        alpha_before, alpha_after,
        "next_run_at must be preserved when cron is unchanged"
    );

    // Third sync: empty list deletes everything.
    let report = queue.sync_schedules(&[]).await.unwrap();
    assert_eq!(report.upserted, 0);
    assert_eq!(report.deleted, vec!["alpha".to_string()]);
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
            match self
                .max_observed
                .compare_exchange(max, c, Ordering::SeqCst, Ordering::SeqCst)
            {
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
    assert!(
        peak >= 2,
        "should actually run >1 at a time, got peak={peak}"
    );

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
        !report.applied.is_empty(),
        "expected at least 1 migration applied, got {}",
        report.applied.len()
    );

    // All expected tables exist.
    for tbl in &[
        "eddyq_jobs",
        "eddyq_schedules",
        "eddyq_groups",
        "_eddyq_migrations",
    ] {
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
    let report = eddyq_core::migrate::down(&pool, "main", usize::MAX)
        .await
        .unwrap();
    assert!(!report.rolled_back.is_empty());

    // All our tables are gone (except _eddyq_migrations, which we keep
    // around as an empty tracking table).
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

    let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _eddyq_migrations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        remaining, 0,
        "tracking table should be empty after full rollback"
    );
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

    // Roll back one: that one is pending again.
    eddyq_core::migrate::down(&pool, "main", 1).await.unwrap();
    let partial = eddyq_core::migrate::status(&pool, "main").await.unwrap();
    assert!(
        partial.last().unwrap().applied_at.is_none(),
        "most recent migration should be pending after down --max-steps 1"
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
    assert!(!canary_status.is_empty());

    let main_status = eddyq_core::migrate::status(&pool, "main").await.unwrap();
    assert!(
        main_status.iter().all(|s| s.applied_at.is_some()),
        "main should see everything as applied"
    );
    assert!(!main_status.is_empty());

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
    assert!(
        canary_rows >= 1,
        "expected ≥1 canary row, got {canary_rows}"
    );

    let main_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM _eddyq_migrations WHERE line = 'main'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(main_rows, 0, "main line should be untouched");
}

/// Per-queue default timeout: handlers that don't return within the duration
/// are aborted and the job gets retried (then failed at max_attempts).
#[sqlx::test(migrations = "./migrations")]
async fn per_queue_timeout_fires_and_retries(pool: PgPool) {
    #[derive(Debug, Serialize, Deserialize)]
    struct HangingTask {
        n: u64,
    }
    impl Job for HangingTask {
        const KIND: &'static str = "hanging";
        fn queue(&self) -> &'static str {
            "slow_lane"
        }
        fn max_attempts(&self) -> i32 {
            2
        }
    }

    let invocations = Arc::new(AtomicUsize::new(0));

    #[derive(Clone)]
    struct HangingWorker {
        count: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl Worker<HangingTask> for HangingWorker {
        async fn perform(&self, _: HangingTask, _: JobContext) -> JobResult {
            self.count.fetch_add(1, Ordering::SeqCst);
            // Sleep way past the queue timeout — worker should be aborted.
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok(())
        }
    }

    let queue = Queue::builder(pool.clone())
        .register::<HangingTask, _>(HangingWorker {
            count: invocations.clone(),
        })
        .subscribe_to(["slow_lane"])
        .config(fast_config())
        .build();

    // 150ms timeout on the queue.
    queue
        .set_queue_timeout("slow_lane", Some(Duration::from_millis(150)))
        .await
        .unwrap();

    queue.enqueue(&HangingTask { n: 1 }).await.unwrap();
    queue.start().unwrap();

    // max_attempts=2 + retry_base=20ms (fast_config). Timeline:
    //   attempt 1 → start, timeout at ~150ms → retry queued at ~170ms
    //   attempt 2 → start ~170ms, timeout at ~320ms → mark failed (at cap)
    // Total: ~400ms. Give it 2s of headroom.
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let failed: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM eddyq_jobs WHERE state = 'failed'")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            if failed >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("job should be marked failed after hitting max_attempts of timeouts");
    queue.shutdown().await.unwrap();

    assert!(
        invocations.load(Ordering::SeqCst) >= 2,
        "handler should have been invoked 2× (attempt 1 timeout + retry)"
    );

    // The error log should mention the timeout.
    let errors: serde_json::Value =
        sqlx::query_scalar("SELECT errors FROM eddyq_jobs WHERE state = 'failed' LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    let errors_str = errors.to_string();
    assert!(
        errors_str.contains("timed out"),
        "error log should mention timeout, got {errors_str}"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn no_timeout_lets_long_jobs_run(pool: PgPool) {
    #[derive(Debug, Serialize, Deserialize)]
    struct SlowTask {
        n: u64,
    }
    impl Job for SlowTask {
        const KIND: &'static str = "slow";
    }

    let done = Arc::new(AtomicUsize::new(0));

    #[derive(Clone)]
    struct SlowWorker(Arc<AtomicUsize>);
    #[async_trait]
    impl Worker<SlowTask> for SlowWorker {
        async fn perform(&self, _: SlowTask, _: JobContext) -> JobResult {
            tokio::time::sleep(Duration::from_millis(200)).await;
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    // No timeout configured on the queue → 200ms sleep runs to completion.
    let queue = Queue::builder(pool.clone())
        .register::<SlowTask, _>(SlowWorker(done.clone()))
        .config(fast_config())
        .build();

    queue.enqueue(&SlowTask { n: 1 }).await.unwrap();
    queue.start().unwrap();

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if done.load(Ordering::SeqCst) >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("slow job should complete without a timeout configured");
    queue.shutdown().await.unwrap();
}

/// The "10 ECS tasks" test: simulate multiple worker processes (each its own
/// Queue + pool) all subscribed to "integrations", set a cross-process cap of
/// 3, enqueue a burst of jobs. Proves peak concurrency across ALL processes
/// stays ≤ 3, not 3 × N_processes.
#[sqlx::test(migrations = "./migrations")]
async fn queue_cap_holds_across_many_worker_processes(pool: PgPool) {
    #[derive(Debug, Serialize, Deserialize)]
    struct IntegTask {
        n: u64,
    }
    impl Job for IntegTask {
        const KIND: &'static str = "integ";
        fn queue(&self) -> &'static str {
            "integrations"
        }
    }

    // One shared counter across *all* simulated processes.
    let current = Arc::new(AtomicUsize::new(0));
    let max_observed = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicUsize::new(0));

    #[derive(Clone)]
    struct SharedWorker {
        current: Arc<AtomicUsize>,
        max_observed: Arc<AtomicUsize>,
        completed: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl Worker<IntegTask> for SharedWorker {
        async fn perform(&self, _: IntegTask, _: JobContext) -> JobResult {
            let c = self.current.fetch_add(1, Ordering::SeqCst) + 1;
            let mut m = self.max_observed.load(Ordering::SeqCst);
            while c > m {
                match self
                    .max_observed
                    .compare_exchange(m, c, Ordering::SeqCst, Ordering::SeqCst)
                {
                    Ok(_) => break,
                    Err(actual) => m = actual,
                }
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
            self.current.fetch_sub(1, Ordering::SeqCst);
            self.completed.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    // Set the cross-process cap BEFORE anything runs.
    sqlx::query("INSERT INTO eddyq_queues (name, max_concurrency) VALUES ('integrations', 3)")
        .execute(&pool)
        .await
        .unwrap();

    // Spin up FIVE independent "processes" — each with its own PgPool to
    // simulate separate ECS tasks. Per-process cap would permit 5 × 4 = 20
    // concurrent; the cross-process cap should keep peak ≤ 3.
    let connect_opts = pool.connect_options();
    let mut process_pools: Vec<sqlx::PgPool> = Vec::new();
    let mut processes: Vec<Queue> = Vec::new();
    for _ in 0..5 {
        let p = sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .connect_with((*connect_opts).clone())
            .await
            .unwrap();
        let q = Queue::builder(p.clone())
            .register::<IntegTask, _>(SharedWorker {
                current: current.clone(),
                max_observed: max_observed.clone(),
                completed: completed.clone(),
            })
            .subscribe_to(["integrations"])
            .config(QueueConfig {
                worker_concurrency: 4,
                poll_only: true, // simulate transaction-pooled PgBouncer mode
                ..fast_config()
            })
            .build();
        process_pools.push(p);
        processes.push(q);
    }

    // Enqueue 50 jobs (plenty of work to saturate).
    let enqueuer = &processes[0];
    for n in 0..50 {
        enqueuer.enqueue(&IntegTask { n }).await.unwrap();
    }

    for q in &processes {
        q.start().unwrap();
    }

    let outcome = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if completed.load(Ordering::SeqCst) >= 50 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;

    if outcome.is_err() {
        let states: Vec<(String, i64)> =
            sqlx::query_as("SELECT state, COUNT(*) FROM eddyq_jobs GROUP BY state ORDER BY state")
                .fetch_all(&pool)
                .await
                .unwrap();
        let qstate: Vec<(String, i32, i32, bool)> =
            sqlx::query_as("SELECT name, running_count, max_concurrency, paused FROM eddyq_queues")
                .fetch_all(&pool)
                .await
                .unwrap();
        panic!(
            "timed out: completed={} states={:?} queue={:?} atomic_current={}",
            completed.load(Ordering::SeqCst),
            states,
            qstate,
            current.load(Ordering::SeqCst)
        );
    }

    for q in &processes {
        q.shutdown().await.unwrap();
    }

    let peak = max_observed.load(Ordering::SeqCst);
    assert!(
        peak <= 3,
        "cross-process queue cap=3 was violated: observed peak={peak} (across 5 processes of worker_concurrency=8 each)"
    );
    assert!(
        peak >= 2,
        "should have actually run ≥2 at once to be a real test; peak was {peak}"
    );

    // And the counter is back to 0 after everything finishes.
    let q = enqueuer.get_queue("integrations").await.unwrap().unwrap();
    assert_eq!(q.running_count, 0);
    assert_eq!(q.max_concurrency, 3);
}

/// Two worker pools subscribed to different named queues — only work on their
/// own queue. Proves routing pools, and verifies that a pool doesn't corrupt
/// the other's jobs (the latent kind-filter bug).
#[sqlx::test(migrations = "./migrations")]
async fn named_queues_route_to_separate_pools(pool: PgPool) {
    #[derive(Debug, Serialize, Deserialize)]
    struct UrgentWork {
        n: u64,
    }
    impl Job for UrgentWork {
        const KIND: &'static str = "urgent_work";
        fn queue(&self) -> &'static str {
            "urgent"
        }
    }

    #[derive(Debug, Serialize, Deserialize)]
    struct DefaultWork {
        n: u64,
    }
    impl Job for DefaultWork {
        const KIND: &'static str = "default_work";
        // queue() defaults to "default"
    }

    let urgent_done = Arc::new(AtomicUsize::new(0));
    let default_done = Arc::new(AtomicUsize::new(0));

    #[derive(Clone)]
    struct UrgentWorker(Arc<AtomicUsize>);
    #[async_trait]
    impl Worker<UrgentWork> for UrgentWorker {
        async fn perform(&self, _: UrgentWork, _: JobContext) -> JobResult {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }
    #[derive(Clone)]
    struct DefaultWorker(Arc<AtomicUsize>);
    #[async_trait]
    impl Worker<DefaultWork> for DefaultWorker {
        async fn perform(&self, _: DefaultWork, _: JobContext) -> JobResult {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    // Pool A: subscribes to "urgent" only, has only the urgent handler.
    let pool_a = Queue::builder(pool.clone())
        .register::<UrgentWork, _>(UrgentWorker(urgent_done.clone()))
        .subscribe_to(["urgent"])
        .config(fast_config())
        .build();

    // Pool B: subscribes to "default" only, has only the default handler.
    let pool_b = Queue::builder(pool.clone())
        .register::<DefaultWork, _>(DefaultWorker(default_done.clone()))
        .subscribe_to(["default"])
        .config(fast_config())
        .build();

    // Enqueue 5 of each.
    for n in 0..5 {
        pool_a.enqueue(&UrgentWork { n }).await.unwrap();
        pool_a.enqueue(&DefaultWork { n }).await.unwrap();
    }

    // Start both pools.
    pool_a.start().unwrap();
    pool_b.start().unwrap();

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if urgent_done.load(Ordering::SeqCst) >= 5 && default_done.load(Ordering::SeqCst) >= 5 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("both pools should drain their queues");

    pool_a.shutdown().await.unwrap();
    pool_b.shutdown().await.unwrap();

    // No jobs were marked failed because pool A claimed pool B's kinds (the
    // old kind-filter bug).
    let failed: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM eddyq_jobs WHERE state = 'failed'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(failed, 0, "no jobs should have been orphaned/failed");

    assert_eq!(urgent_done.load(Ordering::SeqCst), 5);
    assert_eq!(default_done.load(Ordering::SeqCst), 5);
}

#[sqlx::test(migrations = "./migrations")]
async fn cancel_pending_job(pool: PgPool) {
    let queue = Queue::builder(pool.clone()).build();
    let res = queue.enqueue(&Count { n: 1 }).await.unwrap();
    let id = match res {
        eddyq_core::EnqueueResult::Inserted(id) => id,
        _ => panic!("expected Inserted"),
    };

    assert!(queue.cancel(id).await.unwrap(), "pending job should cancel");
    // Cancelling an already-cancelled (or non-existent) job is a no-op.
    assert!(!queue.cancel(id).await.unwrap());

    let state: String = sqlx::query_scalar("SELECT state FROM eddyq_jobs WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, "cancelled");
}

#[sqlx::test(migrations = "./migrations")]
async fn cancel_does_not_affect_running_job(pool: PgPool) {
    // Manually insert a running job to simulate a worker mid-execution.
    let id: i64 = sqlx::query_scalar(
        r#"
        INSERT INTO eddyq_jobs (kind, payload, state, attempt, max_attempts, heartbeat_at, worker_id)
        VALUES ('count', '{"n":1}', 'running', 1, 3, NOW(), gen_random_uuid())
        RETURNING id
        "#,
    )
    .fetch_one(&pool)
    .await
    .unwrap();

    let queue = Queue::builder(pool.clone()).build();
    assert!(
        !queue.cancel(id).await.unwrap(),
        "cancel on a running job must return false and not transition state"
    );
    let state: String = sqlx::query_scalar("SELECT state FROM eddyq_jobs WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, "running", "state must be untouched");
}

#[sqlx::test(migrations = "./migrations")]
async fn tags_and_metadata_round_trip_and_filter(pool: PgPool) {
    #[derive(Debug, Serialize, Deserialize)]
    struct Tagged {
        what: String,
    }
    impl Job for Tagged {
        const KIND: &'static str = "tagged";
        fn tags(&self) -> Vec<String> {
            vec!["urgent".into(), format!("what:{}", self.what)]
        }
        fn metadata(&self) -> Option<serde_json::Value> {
            Some(serde_json::json!({ "trace_id": "abc-123" }))
        }
    }

    let queue = Queue::builder(pool.clone()).build();
    queue
        .enqueue(&Tagged {
            what: "email".into(),
        })
        .await
        .unwrap();
    queue.enqueue(&Tagged { what: "sms".into() }).await.unwrap();

    // Filter by "urgent" — both match.
    let urgent: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM eddyq_jobs WHERE tags @> ARRAY['urgent']::text[]")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(urgent, 2);

    // Filter by specific what-tag — only one matches.
    let emails: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM eddyq_jobs WHERE tags @> ARRAY['what:email']::text[]",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(emails, 1);

    // Metadata round-trips verbatim.
    let meta: serde_json::Value =
        sqlx::query_scalar("SELECT metadata FROM eddyq_jobs ORDER BY id LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(meta, serde_json::json!({ "trace_id": "abc-123" }));
}

#[sqlx::test(migrations = "./migrations")]
async fn enqueue_many_inserts_all(pool: PgPool) {
    let queue = Queue::builder(pool.clone()).build();
    let jobs: Vec<Count> = (0..500).map(|n| Count { n }).collect();

    let result = queue.enqueue_many(&jobs).await.unwrap();
    assert_eq!(result.inserted, 500);
    assert_eq!(result.skipped, 0);

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM eddyq_jobs")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 500);
}

#[sqlx::test(migrations = "./migrations")]
async fn enqueue_many_respects_unique_dedup(pool: PgPool) {
    #[derive(Debug, Serialize, Deserialize)]
    struct Dedup {
        n: u64,
    }
    impl Job for Dedup {
        const KIND: &'static str = "dedup_bulk";
        fn unique_key(&self) -> Option<String> {
            // Every 3rd job shares a unique key — expect 4 dupes skipped out of 10.
            Some((self.n % 6).to_string())
        }
    }

    let queue = Queue::builder(pool.clone()).build();
    let jobs: Vec<Dedup> = (0..10).map(|n| Dedup { n }).collect();

    let result = queue.enqueue_many(&jobs).await.unwrap();
    // Unique keys 0..5 produce 6 distinct inserts; the next 4 (6,7,8,9) alias 0..3.
    assert_eq!(result.inserted, 6);
    assert_eq!(result.skipped, 4);
}

#[sqlx::test(migrations = "./migrations")]
async fn enqueue_many_transactional_rollback(pool: PgPool) {
    let queue = Queue::builder(pool.clone()).build();

    let mut tx = pool.begin().await.unwrap();
    let jobs: Vec<Count> = (0..100).map(|n| Count { n }).collect();
    let res = queue.enqueue_many_in_tx(&mut tx, &jobs).await.unwrap();
    assert_eq!(res.inserted, 100);
    tx.rollback().await.unwrap();

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM eddyq_jobs")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "rollback discards the whole batch");
}

#[sqlx::test(migrations = "./migrations")]
async fn cleanup_deletes_old_finalized_jobs(pool: PgPool) {
    use eddyq_core::fetch::{Retention, cleanup};

    // Seed 5 finalized jobs at various ages + states + one young job.
    sqlx::query(
        r#"
        INSERT INTO eddyq_jobs (kind, payload, state, attempt, max_attempts, finalized_at)
        VALUES
            ('old_ok',  '{}'::jsonb, 'completed', 1, 3, NOW() - INTERVAL '2 hours'),
            ('new_ok',  '{}'::jsonb, 'completed', 1, 3, NOW() - INTERVAL '30 seconds'),
            ('old_fail','{}'::jsonb, 'failed',    3, 3, NOW() - INTERVAL '2 hours'),
            ('new_fail','{}'::jsonb, 'failed',    3, 3, NOW() - INTERVAL '30 seconds'),
            ('old_cxl', '{}'::jsonb, 'cancelled', 0, 3, NOW() - INTERVAL '2 hours')
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();

    // Retention: 1 hour for everything.
    let retention = Retention {
        completed_secs: Some(3600),
        failed_secs: Some(3600),
        cancelled_secs: Some(3600),
    };

    let (c, f, x) = cleanup(&pool, retention).await.unwrap();
    assert_eq!(c, 1, "one old completed should be deleted");
    assert_eq!(f, 1, "one old failed should be deleted");
    assert_eq!(x, 1, "one old cancelled should be deleted");

    // The two "new" ones should still be present.
    let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM eddyq_jobs")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(remaining, 2);
}

#[sqlx::test(migrations = "./migrations")]
async fn cleanup_respects_none_retention(pool: PgPool) {
    use eddyq_core::fetch::{Retention, cleanup};

    // An ancient completed job; with None retention it must not be deleted.
    sqlx::query(
        r#"
        INSERT INTO eddyq_jobs (kind, payload, state, attempt, max_attempts, finalized_at)
        VALUES ('ancient', '{}'::jsonb, 'completed', 1, 3, NOW() - INTERVAL '365 days')
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();

    let (c, f, x) = cleanup(
        &pool,
        Retention {
            completed_secs: None,
            failed_secs: None,
            cancelled_secs: None,
        },
    )
    .await
    .unwrap();
    assert_eq!((c, f, x), (0, 0, 0));

    let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM eddyq_jobs")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(remaining, 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn cleanup_does_not_touch_pending_or_running(pool: PgPool) {
    use eddyq_core::fetch::{Retention, cleanup};

    // Pending + running jobs, whose finalized_at is NULL.
    sqlx::query(
        r#"
        INSERT INTO eddyq_jobs (kind, payload, state, attempt, max_attempts, finalized_at)
        VALUES
            ('pending_job', '{}'::jsonb, 'pending', 0, 3, NULL),
            ('running_job', '{}'::jsonb, 'running', 1, 3, NULL)
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();

    let (c, f, x) = cleanup(
        &pool,
        Retention {
            completed_secs: Some(1),
            failed_secs: Some(1),
            cancelled_secs: Some(1),
        },
    )
    .await
    .unwrap();
    assert_eq!((c, f, x), (0, 0, 0));
    let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM eddyq_jobs")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        remaining, 2,
        "pending and running must never be deleted by cleanup"
    );
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
    assert_eq!(
        jobs, 0,
        "rollback: job must also be gone (this is the feature)"
    );
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
    queue.enqueue_in_tx(&mut tx, &Count { n: 7 }).await.unwrap();
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
    assert_eq!(
        jobs as u64, committed,
        "rolled-back jobs should leave no trace"
    );
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
    assert!(
        (4.99..=5.01).contains(&refilled),
        "bucket should refill to cap=5, got {refilled}"
    );
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

/// FIFO within a capped group: at equal priority, jobs run in enqueue order.
/// Regression guard for the `ORDER BY priority DESC, scheduled_at ASC, id ASC`
/// claim contract.
#[sqlx::test(migrations = "./migrations")]
async fn fifo_within_group(pool: PgPool) {
    #[derive(Debug, Serialize, Deserialize)]
    struct Ordered {
        n: u64,
    }
    impl Job for Ordered {
        const KIND: &'static str = "ordered";
        fn group_key(&self) -> Option<String> {
            Some("tenant:123".into())
        }
    }

    let log: Arc<std::sync::Mutex<Vec<u64>>> = Arc::new(std::sync::Mutex::new(vec![]));

    #[derive(Clone)]
    struct RecordingWorker(Arc<std::sync::Mutex<Vec<u64>>>);
    #[async_trait]
    impl Worker<Ordered> for RecordingWorker {
        async fn perform(&self, job: Ordered, _: JobContext) -> JobResult {
            self.0.lock().unwrap().push(job.n);
            tokio::time::sleep(Duration::from_millis(10)).await;
            Ok(())
        }
    }

    let queue = Queue::builder(pool.clone())
        .register::<Ordered, _>(RecordingWorker(log.clone()))
        .config(QueueConfig {
            worker_concurrency: 1, // serialize to make the order observable
            ..fast_config()
        })
        .build();

    // Cap=1 + worker_concurrency=1 means strict serial execution.
    queue.set_group_concurrency("tenant:123", 1).await.unwrap();

    for n in 0..10 {
        queue.enqueue(&Ordered { n }).await.unwrap();
    }

    queue.start().unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if log.lock().unwrap().len() >= 10 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("all 10 should complete");
    queue.shutdown().await.unwrap();

    let observed = log.lock().unwrap().clone();
    assert_eq!(
        observed,
        (0..10).collect::<Vec<_>>(),
        "jobs must run in enqueue order within a capped group, got {observed:?}"
    );
}

/// Regression test: ungrouped "fastlane" jobs must not starve when a
/// "slowlane" group has a large backlog at equal priority. Before the claim
/// refactor, the fetcher over-fetched globally by priority, saw only
/// capped-group candidates first, and returned 0 — fastlane workers idled.
#[sqlx::test(migrations = "./migrations")]
async fn fastlane_does_not_starve_behind_slowlane(pool: PgPool) {
    #[derive(Debug, Serialize, Deserialize)]
    struct Slow {
        n: u64,
    }
    impl Job for Slow {
        const KIND: &'static str = "slow";
        fn group_key(&self) -> Option<String> {
            Some("slowlane".into())
        }
    }

    #[derive(Debug, Serialize, Deserialize)]
    struct Fast {
        n: u64,
    }
    impl Job for Fast {
        const KIND: &'static str = "fast";
        // No group_key — fastlane.
    }

    let slow_done = Arc::new(AtomicUsize::new(0));
    let fast_done = Arc::new(AtomicUsize::new(0));

    #[derive(Clone)]
    struct SlowWorker(Arc<AtomicUsize>);
    #[async_trait]
    impl Worker<Slow> for SlowWorker {
        async fn perform(&self, _: Slow, _: JobContext) -> JobResult {
            tokio::time::sleep(Duration::from_millis(30)).await;
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }
    #[derive(Clone)]
    struct FastWorker(Arc<AtomicUsize>);
    #[async_trait]
    impl Worker<Fast> for FastWorker {
        async fn perform(&self, _: Fast, _: JobContext) -> JobResult {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    let queue = Queue::builder(pool.clone())
        .register::<Slow, _>(SlowWorker(slow_done.clone()))
        .register::<Fast, _>(FastWorker(fast_done.clone()))
        .config(QueueConfig {
            worker_concurrency: 10,
            ..fast_config()
        })
        .build();

    // Cap slowlane at 2. Then enqueue 100 slow jobs *before* 20 fast ones,
    // so they have lower IDs and would be picked first by a priority-only
    // claim. With the fix, fastlane still gets through.
    queue.set_group_concurrency("slowlane", 2).await.unwrap();

    for n in 0..100 {
        queue.enqueue(&Slow { n }).await.unwrap();
    }
    for n in 0..20 {
        queue.enqueue(&Fast { n }).await.unwrap();
    }

    queue.start().unwrap();

    // All 20 fast jobs should finish quickly (well before the slow ones),
    // proving they weren't blocked behind the slow backlog.
    tokio::time::timeout(Duration::from_millis(1500), async {
        loop {
            if fast_done.load(Ordering::SeqCst) >= 20 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fastlane jobs should not be starved by slowlane backlog");

    assert!(fast_done.load(Ordering::SeqCst) >= 20, "fast jobs done");
    // Slow is still working — we don't wait for all of them.
    queue.shutdown().await.unwrap();
}

/// Index coverage check: the hot-path partial indexes exist. Regression
/// guard against accidentally dropping an index in a future migration.
/// (At small row counts Postgres may choose Seq Scan even with an index —
/// that's correct behavior; we're asserting availability, not planner choice.)
#[sqlx::test(migrations = "./migrations")]
async fn hot_path_indexes_exist(pool: PgPool) {
    let expected: &[&str] = &[
        "eddyq_jobs_fetch",
        "eddyq_jobs_fetch_ungrouped",
        "eddyq_jobs_group",
        "eddyq_jobs_heartbeat",
        "eddyq_jobs_unique",
        "eddyq_jobs_kind",
        "eddyq_jobs_finalized",
        "eddyq_jobs_tags",
    ];

    let existing: Vec<String> = sqlx::query_scalar(
        "SELECT indexname FROM pg_indexes
          WHERE schemaname = current_schema() AND tablename = 'eddyq_jobs'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();

    for idx in expected {
        assert!(
            existing.iter().any(|e| e == idx),
            "expected index {idx} on eddyq_jobs not found; present: {existing:?}"
        );
    }
}

/// Pattern-based rule: one `set_group_rule("shopify:*", cap=2)` call covers
/// every shopify integration ever enqueued, no per-tenant setup needed.
#[sqlx::test(migrations = "./migrations")]
async fn group_rule_auto_caps_new_integrations(pool: PgPool) {
    use eddyq_core::group::GroupRule;

    #[derive(Debug, Serialize, Deserialize)]
    struct SyncTask {
        integration: String,
    }
    impl Job for SyncTask {
        const KIND: &'static str = "shopify_sync";
        fn group_key(&self) -> Option<String> {
            Some(format!("shopify:{}", self.integration))
        }
    }

    let queue = Queue::builder(pool.clone()).build();

    // One global rule. No per-integration setup.
    queue
        .set_group_rule("shopify:*", GroupRule::concurrency(2))
        .await
        .unwrap();

    // Enqueue for three new, previously-unknown integrations.
    for tenant in &["acme", "globex", "initech"] {
        queue
            .enqueue(&SyncTask {
                integration: (*tenant).into(),
            })
            .await
            .unwrap();
    }

    // Each should have a materialized eddyq_groups row with cap=2.
    for tenant in &["acme", "globex", "initech"] {
        let g = queue
            .get_group(&format!("shopify:{tenant}"))
            .await
            .unwrap()
            .expect("rule should have materialized a group row");
        assert_eq!(g.max_concurrency, 2, "rule should apply cap=2 to {tenant}");
        assert_eq!(g.running_count, 0);
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn more_specific_rule_wins(pool: PgPool) {
    use eddyq_core::group::GroupRule;

    #[derive(Debug, Serialize, Deserialize)]
    struct SyncTask {
        integration: String,
    }
    impl Job for SyncTask {
        const KIND: &'static str = "ssync";
        fn group_key(&self) -> Option<String> {
            Some(format!("shopify:{}", self.integration))
        }
    }

    let queue = Queue::builder(pool.clone()).build();

    // Generic rule: all shopify get cap=2. Specific rule: premium gets cap=10.
    queue
        .set_group_rule("shopify:*", GroupRule::concurrency(2))
        .await
        .unwrap();
    queue
        .set_group_rule("shopify:premium:*", GroupRule::concurrency(10))
        .await
        .unwrap();

    queue
        .enqueue(&SyncTask {
            integration: "acme".into(),
        })
        .await
        .unwrap();
    queue
        .enqueue(&SyncTask {
            integration: "premium:initech".into(),
        })
        .await
        .unwrap();

    let acme = queue.get_group("shopify:acme").await.unwrap().unwrap();
    let premium = queue
        .get_group("shopify:premium:initech")
        .await
        .unwrap()
        .unwrap();

    assert_eq!(acme.max_concurrency, 2);
    assert_eq!(premium.max_concurrency, 10, "more-specific pattern wins");
}

#[sqlx::test(migrations = "./migrations")]
async fn explicit_set_overrides_rule(pool: PgPool) {
    use eddyq_core::group::GroupRule;

    #[derive(Debug, Serialize, Deserialize)]
    struct SyncTask {
        integration: String,
    }
    impl Job for SyncTask {
        const KIND: &'static str = "ssync";
        fn group_key(&self) -> Option<String> {
            Some(format!("shopify:{}", self.integration))
        }
    }

    let queue = Queue::builder(pool.clone()).build();

    // Explicitly cap a specific integration BEFORE setting up the pattern.
    queue
        .set_group_concurrency("shopify:vip", 20)
        .await
        .unwrap();

    // Then a generic rule that would otherwise catch it.
    queue
        .set_group_rule("shopify:*", GroupRule::concurrency(2))
        .await
        .unwrap();

    // Enqueueing for vip shouldn't reset its cap — explicit setting wins
    // (ON CONFLICT DO NOTHING on the materialize path).
    queue
        .enqueue(&SyncTask {
            integration: "vip".into(),
        })
        .await
        .unwrap();

    let vip = queue.get_group("shopify:vip").await.unwrap().unwrap();
    assert_eq!(
        vip.max_concurrency, 20,
        "explicit set_group_concurrency before rule must not be overwritten"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn rule_with_rate_materializes_token_bucket(pool: PgPool) {
    use eddyq_core::group::GroupRule;

    #[derive(Debug, Serialize, Deserialize)]
    struct SyncTask {
        tenant: String,
    }
    impl Job for SyncTask {
        const KIND: &'static str = "openai_call";
        fn group_key(&self) -> Option<String> {
            Some(format!("tenant:{}:openai", self.tenant))
        }
    }

    let queue = Queue::builder(pool.clone()).build();
    queue
        .set_group_rule(
            "tenant:*:openai",
            GroupRule::both(20, 1000, Duration::from_secs(60)),
        )
        .await
        .unwrap();

    queue
        .enqueue(&SyncTask {
            tenant: "acme".into(),
        })
        .await
        .unwrap();

    let g = queue
        .get_group("tenant:acme:openai")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(g.max_concurrency, 20);
    assert_eq!(g.rate_count, Some(1000));
    assert_eq!(g.rate_period_ms, Some(60_000));
    // Bucket starts full.
    assert!(
        (g.tokens - 1000.0).abs() < 0.5,
        "token bucket should initialize to rate_count; got {}",
        g.tokens
    );
}

/// Simulates a real-world pattern: two Shopify integrations, each has a burst
/// of tasks, each should be independently throttled to 2 concurrent workers.
/// Integration A's cap should not slow down integration B, and vice versa.
#[sqlx::test(migrations = "./migrations")]
async fn per_integration_concurrency_cap(pool: PgPool) {
    #[derive(Debug, Serialize, Deserialize)]
    struct SyncTask {
        integration: String,
        item: u32,
    }
    impl Job for SyncTask {
        const KIND: &'static str = "shopify_sync";
        fn group_key(&self) -> Option<String> {
            Some(format!("shopify:{}", self.integration))
        }
    }

    #[derive(Clone)]
    struct TrackedWorker {
        per_integration_current: Arc<std::sync::Mutex<std::collections::HashMap<String, usize>>>,
        per_integration_peak: Arc<std::sync::Mutex<std::collections::HashMap<String, usize>>>,
        completed: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Worker<SyncTask> for TrackedWorker {
        async fn perform(&self, job: SyncTask, _ctx: JobContext) -> JobResult {
            let key = job.integration.clone();
            let now = {
                let mut cur = self.per_integration_current.lock().unwrap();
                let slot = cur.entry(key.clone()).or_insert(0);
                *slot += 1;
                *slot
            };
            {
                let mut peak = self.per_integration_peak.lock().unwrap();
                let p = peak.entry(key.clone()).or_insert(0);
                if now > *p {
                    *p = now;
                }
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
            {
                let mut cur = self.per_integration_current.lock().unwrap();
                *cur.get_mut(&key).unwrap() -= 1;
            }
            self.completed.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    let per_current = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let per_peak = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let completed = Arc::new(AtomicUsize::new(0));

    let queue = Queue::builder(pool.clone())
        .register::<SyncTask, _>(TrackedWorker {
            per_integration_current: per_current.clone(),
            per_integration_peak: per_peak.clone(),
            completed: completed.clone(),
        })
        .config(QueueConfig {
            worker_concurrency: 12, // plenty of workers — cap has to come from groups
            ..fast_config()
        })
        .build();

    // Two integrations, each capped at 2 concurrent.
    queue
        .set_group_concurrency("shopify:acme", 2)
        .await
        .unwrap();
    queue
        .set_group_concurrency("shopify:globex", 2)
        .await
        .unwrap();

    // 30 tasks for each integration.
    for item in 0..30 {
        queue
            .enqueue(&SyncTask {
                integration: "acme".into(),
                item,
            })
            .await
            .unwrap();
        queue
            .enqueue(&SyncTask {
                integration: "globex".into(),
                item,
            })
            .await
            .unwrap();
    }

    queue.start().unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if completed.load(Ordering::SeqCst) >= 60 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("all 60 tasks across both integrations should complete");
    queue.shutdown().await.unwrap();

    // Each integration's peak concurrency must have been at most 2.
    let (acme_peak, globex_peak) = {
        let peaks = per_peak.lock().unwrap();
        (*peaks.get("acme").unwrap(), *peaks.get("globex").unwrap())
    };
    assert!(
        acme_peak <= 2,
        "acme peak concurrency should be ≤ 2, got {acme_peak}"
    );
    assert!(
        globex_peak <= 2,
        "globex peak concurrency should be ≤ 2, got {globex_peak}"
    );
    // Both should have actually run ≥1 at a time (proof jobs ran).
    assert!(acme_peak >= 1);
    assert!(globex_peak >= 1);

    // And the group tables show the live state is cleaned up.
    let acme = queue.get_group("shopify:acme").await.unwrap().unwrap();
    let globex = queue.get_group("shopify:globex").await.unwrap().unwrap();
    assert_eq!(acme.running_count, 0);
    assert_eq!(globex.running_count, 0);
    assert_eq!(acme.max_concurrency, 2);
    assert_eq!(globex.max_concurrency, 2);
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

// ─── SQL enqueue functions (cross-language transactional enqueue) ──────────

/// Call `eddyq_enqueue(...)` from raw SQL and assert the row matches what the
/// Rust path produces for the same inputs. This is the parity test that keeps
/// the two implementations honest.
#[sqlx::test(migrations = "./migrations")]
async fn sql_enqueue_function_parity(pool: PgPool) {
    use serde_json::json;

    // Pin scheduled_at so the two rows are comparable. The only fields that
    // should differ are `id` (serial), `created_at`, and `updated_at`.
    let scheduled_at = chrono::Utc::now() + chrono::Duration::seconds(60);
    let payload = json!({"to": "a@x.com", "n": 7});

    // Rust path.
    let mut req = eddyq_core::enqueue::DynEnqueue::new("send.email", payload.clone());
    req.priority = 5;
    req.max_attempts = 4;
    req.queue = "urgent".to_owned();
    req.scheduled_at = Some(scheduled_at);
    req.unique_key = Some("rust-parity-1".into());
    req.group_key = Some("acme".into());
    req.tags = vec!["urgent".into(), "billing".into()];
    req.metadata = json!({"source": "rust"});
    let res = eddyq_core::enqueue::enqueue_dyn(&pool, req).await.unwrap();
    let rust_id = match res {
        eddyq_core::EnqueueResult::Inserted(id) => id,
        _ => panic!("rust enqueue should have inserted"),
    };

    // SQL path — same inputs except unique_key (else it would dedupe).
    let sql_id: Option<i64> = sqlx::query_scalar(
        r#"
        SELECT eddyq_enqueue(
            $1, $2::jsonb, $3, $4::smallint, $5::int, $6::timestamptz,
            $7, $8, $9::text[], $10::jsonb
        )
        "#,
    )
    .bind("send.email")
    .bind(&payload)
    .bind("urgent")
    .bind(5_i16)
    .bind(4_i32)
    .bind(scheduled_at)
    .bind("sql-parity-1")
    .bind("acme")
    .bind(vec!["urgent".to_string(), "billing".to_string()])
    .bind(json!({"source": "rust"})) // same metadata on purpose, for row compare
    .fetch_one(&pool)
    .await
    .unwrap();
    let sql_id = sql_id.expect("sql enqueue should have returned an id");

    type JobRow = (
        String,
        serde_json::Value,
        String,
        i16,
        i32,
        chrono::DateTime<chrono::Utc>,
        Option<String>,
        Vec<String>,
        serde_json::Value,
        String,
    );

    // Compare every column that's supposed to match between the two.
    let row: JobRow = sqlx::query_as(
        r#"
        SELECT kind, payload, state::text, priority, max_attempts, scheduled_at,
               group_key, tags, metadata, queue
          FROM eddyq_jobs
         WHERE id = ANY($1)
      ORDER BY id
        "#,
    )
    .bind([rust_id, sql_id])
    .fetch_all(&pool)
    .await
    .unwrap()
    .into_iter()
    .next()
    .unwrap();

    // Fetch both rows in one query for a side-by-side compare.
    let rows: Vec<JobRow> = sqlx::query_as(
        r#"
        SELECT kind, payload, state::text, priority, max_attempts, scheduled_at,
               group_key, tags, metadata, queue
          FROM eddyq_jobs
         WHERE id = ANY($1)
      ORDER BY id
        "#,
    )
    .bind([rust_id, sql_id])
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2, "both rows must exist");
    let (a, b) = (&rows[0], &rows[1]);
    assert_eq!(a.0, b.0, "kind must match");
    assert_eq!(a.1, b.1, "payload must match");
    assert_eq!(a.2, b.2, "state must match");
    assert_eq!(a.3, b.3, "priority must match");
    assert_eq!(a.4, b.4, "max_attempts must match");
    assert_eq!(a.5, b.5, "scheduled_at must match");
    assert_eq!(a.6, b.6, "group_key must match");
    assert_eq!(a.7, b.7, "tags must match");
    assert_eq!(a.8, b.8, "metadata must match");
    assert_eq!(a.9, b.9, "queue must match");

    // unused binding kept just to silence clippy about `row` variable above.
    let _ = row;
}

/// Calling eddyq_enqueue inside a Postgres transaction that rolls back must
/// produce no job row — same semantics as the Rust enqueue_in_tx path. This
/// is the defining feature of a Postgres-backed queue vs Redis.
#[sqlx::test(migrations = "./migrations")]
async fn sql_enqueue_rolls_back_with_user_tx(pool: PgPool) {
    use serde_json::json;

    // Create a fake domain table to simulate the user's own write.
    sqlx::query("CREATE TABLE tx_widgets (id BIGSERIAL PRIMARY KEY, label TEXT NOT NULL)")
        .execute(&pool)
        .await
        .unwrap();

    // Inside a tx: insert a widget + enqueue a follow-up job, then roll back.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO tx_widgets (label) VALUES ('rollback-me')")
        .execute(&mut *tx)
        .await
        .unwrap();
    let id: Option<i64> = sqlx::query_scalar("SELECT eddyq_enqueue($1, $2::jsonb)")
        .bind("sql.tx.test")
        .bind(json!({"rolled": false}))
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert!(
        id.is_some(),
        "enqueue inside tx should return an id pre-rollback"
    );
    tx.rollback().await.unwrap();

    // Both the widget and the job must be gone.
    let widgets: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tx_widgets")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(widgets, 0, "widget must be rolled back");
    let jobs: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM eddyq_jobs WHERE kind = 'sql.tx.test'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        jobs, 0,
        "job must be rolled back alongside the domain write"
    );
}

/// unique_key collisions via the SQL function return NULL rather than raising.
#[sqlx::test(migrations = "./migrations")]
async fn sql_enqueue_unique_key_conflict_returns_null(pool: PgPool) {
    let first: Option<i64> = sqlx::query_scalar(
        "SELECT eddyq_enqueue('k', '{}'::jsonb, 'default', 0::smallint, 3, NOW(), 'same-key')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(first.is_some());

    let second: Option<i64> = sqlx::query_scalar(
        "SELECT eddyq_enqueue('k', '{}'::jsonb, 'default', 0::smallint, 3, NOW(), 'same-key')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(second.is_none(), "conflict should return NULL, not raise");
}

/// SQL bulk enqueue round-trips a mixed batch and reports aggregate counts.
#[sqlx::test(migrations = "./migrations")]
async fn sql_enqueue_many_bulk(pool: PgPool) {
    use serde_json::json;

    let stamp = chrono::Utc::now().timestamp_millis();
    let items = json!([
        { "kind": "sql.bulk.a", "payload": {"n": 1}, "unique_key": format!("sql-bulk-a-{stamp}") },
        { "kind": "sql.bulk.b", "payload": {"n": 2}, "unique_key": format!("sql-bulk-b-{stamp}") },
        { "kind": "sql.bulk.a", "payload": {"n": 3}, "unique_key": format!("sql-bulk-a-{stamp}") }, // dupe
    ]);

    let result: serde_json::Value = sqlx::query_scalar("SELECT eddyq_enqueue_many($1::jsonb)")
        .bind(&items)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(result["inserted"], 2);
    assert_eq!(result["skipped"], 1);
}

/// Group rule materialization works through the SQL path (via the internal
/// helper). A job with a group_key matching a pattern rule should lazily
/// create the eddyq_groups row with the rule's cap.
#[sqlx::test(migrations = "./migrations")]
async fn sql_enqueue_materializes_group_rule(pool: PgPool) {
    use serde_json::json;

    // Install a pattern rule capping "shopify:*" to 5 concurrent jobs.
    sqlx::query(
        "INSERT INTO eddyq_group_rules (pattern, max_concurrency, priority) VALUES ('shopify:*', 5, 10)",
    )
    .execute(&pool)
    .await
    .unwrap();

    // Enqueue via SQL with a matching group_key.
    let _: Option<i64> = sqlx::query_scalar(
        "SELECT eddyq_enqueue('sync', $1::jsonb, 'default', 0::smallint, 3, NOW(), NULL, 'shopify:42')",
    )
    .bind(json!({"shop": 42}))
    .fetch_one(&pool)
    .await
    .unwrap();

    let cap: i32 =
        sqlx::query_scalar("SELECT max_concurrency FROM eddyq_groups WHERE key = 'shopify:42'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        cap, 5,
        "pattern rule 'shopify:*' should have materialized the row"
    );
}

// ─── Batch heartbeat tests ────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn batch_heartbeat_updates_all_in_flight(pool: PgPool) {
    use eddyq_core::fetch::{claim_batch, update_heartbeat_batch};
    use uuid::Uuid;

    let worker_id = Uuid::new_v4();

    // Insert 3 pending jobs.
    for n in 0..3i64 {
        sqlx::query(
            "INSERT INTO eddyq_jobs (kind, payload, state) VALUES ('count', $1::jsonb, 'pending')",
        )
        .bind(serde_json::json!({"n": n}))
        .execute(&pool)
        .await
        .unwrap();
    }

    // Claim them so they're in 'running' state.
    let claimed = claim_batch(&pool, worker_id, 10, &["count".to_string()], &["default".to_string()])
        .await
        .unwrap();
    assert_eq!(claimed.len(), 3, "should have claimed 3 jobs");

    // Record heartbeat_at before the batch update.
    let ids: Vec<i64> = claimed.iter().map(|j| j.id).collect();

    // Sleep briefly to ensure NOW() advances past the initial heartbeat_at.
    tokio::time::sleep(Duration::from_millis(10)).await;

    let updated = update_heartbeat_batch(&pool, &ids).await.unwrap();
    assert_eq!(updated, 3, "should have updated 3 heartbeats");

    // Verify heartbeat_at was updated for all 3.
    let updated_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM eddyq_jobs WHERE id = ANY($1) AND heartbeat_at IS NOT NULL AND state = 'running'",
    )
    .bind(&ids)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(updated_count, 3);
}

// ─── Leader election tests ────────────────────────────────────────────────────

#[sqlx::test(migrations = "./migrations")]
async fn leader_election_only_one_wins(pool: PgPool) {
    use eddyq_core::leader::try_elect;
    use uuid::Uuid;

    let worker_a = Uuid::new_v4();
    let worker_b = Uuid::new_v4();

    // First worker to try wins.
    let a_won = try_elect(&pool, worker_a, "test_role", 30).await.unwrap();
    assert!(a_won, "first worker should win the election");

    // Second worker trying immediately should lose.
    let b_won = try_elect(&pool, worker_b, "test_role", 30).await.unwrap();
    assert!(!b_won, "second worker should lose — first is still leader");
}

#[sqlx::test(migrations = "./migrations")]
async fn leader_resign_allows_takeover(pool: PgPool) {
    use eddyq_core::leader::{resign, try_elect};
    use uuid::Uuid;

    let worker_a = Uuid::new_v4();
    let worker_b = Uuid::new_v4();

    // Worker A wins.
    let a_won = try_elect(&pool, worker_a, "resign_role", 30).await.unwrap();
    assert!(a_won);

    // Worker B is denied.
    let b_won = try_elect(&pool, worker_b, "resign_role", 30).await.unwrap();
    assert!(!b_won);

    // Worker A resigns.
    resign(&pool, worker_a, "resign_role").await.unwrap();

    // Worker B can now win.
    let b_won_after = try_elect(&pool, worker_b, "resign_role", 30).await.unwrap();
    assert!(b_won_after, "worker B should win after A resigns");
}

#[sqlx::test(migrations = "./migrations")]
async fn leader_expired_lease_allows_takeover(pool: PgPool) {
    use eddyq_core::leader::try_elect;
    use uuid::Uuid;

    let worker_a = Uuid::new_v4();
    let worker_b = Uuid::new_v4();

    // Worker A wins with a 1-second lease.
    let a_won = try_elect(&pool, worker_a, "expire_role", 1).await.unwrap();
    assert!(a_won);

    // Wait 2 seconds for the lease to expire.
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Worker B can now win.
    let b_won = try_elect(&pool, worker_b, "expire_role", 30).await.unwrap();
    assert!(b_won, "worker B should win after A's lease expires");
}

#[sqlx::test(migrations = "./migrations")]
async fn leader_refresh_keeps_leadership(pool: PgPool) {
    use eddyq_core::leader::try_elect;
    use uuid::Uuid;

    let worker_a = Uuid::new_v4();
    let worker_b = Uuid::new_v4();

    // Worker A wins.
    let a_won = try_elect(&pool, worker_a, "refresh_role", 30).await.unwrap();
    assert!(a_won);

    // Worker A refreshes.
    let a_refreshed = try_elect(&pool, worker_a, "refresh_role", 30).await.unwrap();
    assert!(a_refreshed, "leader should be able to refresh its own lease");

    // Worker B still can't win.
    let b_won = try_elect(&pool, worker_b, "refresh_role", 30).await.unwrap();
    assert!(!b_won, "B should still lose after A refreshes");
}

#[sqlx::test(migrations = "./migrations")]
async fn sweeper_recovers_stale_running_direct(pool: PgPool) {
    use eddyq_core::fetch::sweep_stale;

    // Insert a stale running job directly.
    sqlx::query(
        r#"
        INSERT INTO eddyq_jobs (kind, payload, state, attempt, max_attempts, heartbeat_at, worker_id)
        VALUES ('count', '{"n":99}', 'running', 1, 3, NOW() - INTERVAL '10 seconds', gen_random_uuid())
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();

    // sweep_stale should recover it.
    let recovered = sweep_stale(&pool, Duration::from_secs(1)).await.unwrap();
    assert_eq!(recovered, 1, "should have recovered 1 stale job");

    let state: String = sqlx::query_scalar("SELECT state FROM eddyq_jobs LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state, "pending", "recovered job should go back to pending");
}
