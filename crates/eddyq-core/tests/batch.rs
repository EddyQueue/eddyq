//! Batch primitive integration tests. Require Postgres at `DATABASE_URL`.
//!
//!     just db-up
//!     DATABASE_URL=postgres://eddyq:eddyq@localhost:5433/eddyq_dev \
//!         cargo test -p eddyq-core --test batch -- --test-threads=1

#![allow(clippy::unreadable_literal, clippy::too_many_lines)]

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use eddyq_core::{
    DynEnqueue, Job, JobContext, JobResult, Queue, QueueConfig, Worker, async_trait,
    batch::{enqueue_batch, enqueue_batch_in_tx, BatchOptions},
    fetch::{mark_completed, sweep_stale},
};
use uuid::Uuid;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

fn item(kind: &str, n: u64) -> DynEnqueue {
    DynEnqueue::new(kind, serde_json::json!({ "n": n }))
}

#[derive(Debug, Serialize, Deserialize)]
struct BatchItem {
    n: u64,
}

impl Job for BatchItem {
    const KIND: &'static str = "batch.item";
}

#[derive(Debug, Serialize, Deserialize)]
struct BatchFlaky {
    fail_until_attempt: i32,
}

impl Job for BatchFlaky {
    const KIND: &'static str = "batch.flaky";
}

struct BatchFlakyWorker {
    counter: Arc<AtomicUsize>,
}

#[async_trait]
impl Worker<BatchFlaky> for BatchFlakyWorker {
    async fn perform(&self, job: BatchFlaky, ctx: JobContext) -> JobResult {
        if ctx.attempt < job.fail_until_attempt {
            anyhow::bail!("flaky failure on attempt {}", ctx.attempt);
        }
        self.counter.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct BatchDone {}

impl Job for BatchDone {
    const KIND: &'static str = "batch.done";
}

#[derive(Clone)]
struct BatchItemWorker {
    counter: Arc<AtomicUsize>,
    fail_n: Option<u64>,
}

#[async_trait]
impl Worker<BatchItem> for BatchItemWorker {
    async fn perform(&self, job: BatchItem, _ctx: JobContext) -> JobResult {
        if Some(job.n) == self.fail_n {
            anyhow::bail!("forced terminal failure on n={}", job.n);
        }
        self.counter.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[derive(Clone)]
struct BatchDoneWorker {
    fired: Arc<AtomicUsize>,
}

#[async_trait]
impl Worker<BatchDone> for BatchDoneWorker {
    async fn perform(&self, _job: BatchDone, _ctx: JobContext) -> JobResult {
        self.fired.fetch_add(1, Ordering::SeqCst);
        Ok(())
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
        stale_after: Duration::from_secs(10),
        retry_base: Duration::from_millis(20),
        retry_max: Duration::from_millis(200),
        scheduler_interval: Duration::from_millis(100),
        cleanup_interval: Duration::from_secs(60),
        completed_retention: None,
        failed_retention: None,
        cancelled_retention: None,
        poll_only: false,
        leader_lease_secs: 30,
    }
}

async fn poll_until<F, Fut>(label: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if check().await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for: {label}"));
}

#[sqlx::test(migrations = "./migrations")]
async fn batch_basic_enqueue_lands(pool: PgPool) {
    let items = (0..3u64).map(|n| item("count", n)).collect::<Vec<_>>();
    let result = enqueue_batch(&pool, items, BatchOptions::default())
        .await
        .unwrap();

    assert_eq!(result.inserted, 3);
    assert_eq!(result.skipped, 0);

    let (total, state, on_complete): (i32, String, Option<serde_json::Value>) = sqlx::query_as(
        "SELECT total, state, on_complete FROM eddyq_batches WHERE id = $1",
    )
    .bind(result.batch_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(total, 3);
    assert_eq!(state, "pending");
    assert!(on_complete.is_none());

    let (jobs_in_batch,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM eddyq_jobs WHERE batch_id = $1")
            .bind(result.batch_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(jobs_in_batch, 3);
}

#[sqlx::test(migrations = "./migrations")]
async fn batch_in_tx_rolls_back(pool: PgPool) {
    let items = (0..3u64).map(|n| item("count", n)).collect::<Vec<_>>();

    let mut tx = pool.begin().await.unwrap();
    let result = enqueue_batch_in_tx(&mut tx, items, BatchOptions::default())
        .await
        .unwrap();
    assert_eq!(result.inserted, 3);
    drop(tx);

    let (batch_count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM eddyq_batches")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(batch_count, 0);
    let (job_count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM eddyq_jobs")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(job_count, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn batch_empty_fires_immediately(pool: PgPool) {
    let on_complete = DynEnqueue::new("batch.done", serde_json::json!({ "by": "test" }));
    let result = enqueue_batch(
        &pool,
        Vec::new(),
        BatchOptions {
            on_complete: Some(on_complete),
            metadata: serde_json::Value::Null,
        },
    )
    .await
    .unwrap();
    assert_eq!(result.inserted, 0);
    assert_eq!(result.skipped, 0);

    let (total, state, finalized_at): (i32, String, Option<chrono::DateTime<chrono::Utc>>) =
        sqlx::query_as(
            "SELECT total, state, finalized_at FROM eddyq_batches WHERE id = $1",
        )
        .bind(result.batch_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(total, 0);
    assert_eq!(state, "complete");
    assert!(finalized_at.is_some());

    let (callback_kind, payload): (String, serde_json::Value) = sqlx::query_as(
        "SELECT kind, payload FROM eddyq_jobs WHERE unique_key = $1",
    )
    .bind(format!("eddyq.batch.{}.callback", result.batch_id))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(callback_kind, "batch.done");
    let envelope = payload.get("_eddyq_batch").unwrap();
    assert_eq!(envelope.get("total").unwrap(), &serde_json::json!(0));
    assert_eq!(envelope.get("completed").unwrap(), &serde_json::json!(0));
    assert_eq!(envelope.get("batchId").unwrap(), &serde_json::json!(result.batch_id));
    assert_eq!(payload.get("by").unwrap(), &serde_json::json!("test"));
}

#[sqlx::test(migrations = "./migrations")]
async fn batch_all_skipped_unique_key_fires(pool: PgPool) {
    // Pre-insert a job that will conflict on (kind, unique_key) with the batch
    // item. ON CONFLICT DO NOTHING means the batch's insert is skipped.
    let pre = DynEnqueue {
        unique_key: Some("dup".into()),
        ..DynEnqueue::new("count", serde_json::json!({ "n": 99 }))
    };
    eddyq_core::enqueue::enqueue_dyn(&pool, pre).await.unwrap();

    let on_complete = DynEnqueue::new("batch.done", serde_json::json!({}));
    let dupe_item = DynEnqueue {
        unique_key: Some("dup".into()),
        ..DynEnqueue::new("count", serde_json::json!({ "n": 1 }))
    };
    let result = enqueue_batch(
        &pool,
        vec![dupe_item],
        BatchOptions {
            on_complete: Some(on_complete),
            metadata: serde_json::Value::Null,
        },
    )
    .await
    .unwrap();

    assert_eq!(result.inserted, 0);
    assert_eq!(result.skipped, 1);

    let (total, state): (i32, String) =
        sqlx::query_as("SELECT total, state FROM eddyq_batches WHERE id = $1")
            .bind(result.batch_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(total, 0);
    assert_eq!(state, "complete");

    let (callback_count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM eddyq_jobs WHERE unique_key = $1",
    )
    .bind(format!("eddyq.batch.{}.callback", result.batch_id))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(callback_count, 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn batch_all_complete_fires_on_complete(pool: PgPool) {
    let counter = Arc::new(AtomicUsize::new(0));
    let fired = Arc::new(AtomicUsize::new(0));
    let queue = Queue::builder(pool.clone())
        .register::<BatchItem, _>(BatchItemWorker {
            counter: counter.clone(),
            fail_n: None,
        })
        .register::<BatchDone, _>(BatchDoneWorker {
            fired: fired.clone(),
        })
        .config(fast_config())
        .build();

    let items: Vec<DynEnqueue> = (0..5u64).map(|n| item("batch.item", n)).collect();
    let on_complete = DynEnqueue::new("batch.done", serde_json::json!({ "marker": "ok" }));
    let result = enqueue_batch(
        &pool,
        items,
        BatchOptions {
            on_complete: Some(on_complete),
            metadata: serde_json::Value::Null,
        },
    )
    .await
    .unwrap();
    assert_eq!(result.inserted, 5);

    queue.start().unwrap();
    poll_until("5 items + 1 callback fire", || async {
        counter.load(Ordering::SeqCst) >= 5 && fired.load(Ordering::SeqCst) >= 1
    })
    .await;
    queue.shutdown().await.unwrap();

    assert_eq!(fired.load(Ordering::SeqCst), 1, "callback fires exactly once");

    let (state, completed, failed, cancelled): (String, i32, i32, i32) = sqlx::query_as(
        "SELECT state, completed, failed, cancelled FROM eddyq_batches WHERE id = $1",
    )
    .bind(result.batch_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(state, "complete");
    assert_eq!(completed, 5);
    assert_eq!(failed, 0);
    assert_eq!(cancelled, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn batch_callback_idempotent_under_concurrent_settle(pool: PgPool) {
    // Two jobs in a batch, both manually transitioned to 'running' with
    // distinct worker_ids. We then race two `mark_completed` calls — the last
    // terminal-transition is the one that fires the callback, but BOTH could
    // arrive ~simultaneously. Only one callback row must result.
    let items: Vec<DynEnqueue> = (0..2u64).map(|n| item("batch.item", n)).collect();
    let on_complete = DynEnqueue::new("batch.done", serde_json::json!({}));
    let result = enqueue_batch(
        &pool,
        items,
        BatchOptions {
            on_complete: Some(on_complete),
            metadata: serde_json::Value::Null,
        },
    )
    .await
    .unwrap();
    assert_eq!(result.inserted, 2);

    let job_ids: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM eddyq_jobs WHERE batch_id = $1 ORDER BY id ASC",
    )
    .bind(result.batch_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(job_ids.len(), 2);
    let worker_a = Uuid::new_v4();
    let worker_b = Uuid::new_v4();
    sqlx::query(
        "UPDATE eddyq_jobs SET state='running', worker_id=$1, heartbeat_at=NOW() WHERE id=$2",
    )
    .bind(worker_a)
    .bind(job_ids[0])
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE eddyq_jobs SET state='running', worker_id=$1, heartbeat_at=NOW() WHERE id=$2",
    )
    .bind(worker_b)
    .bind(job_ids[1])
    .execute(&pool)
    .await
    .unwrap();

    let pa = pool.clone();
    let pb = pool.clone();
    let id_a = job_ids[0];
    let id_b = job_ids[1];
    let (ra, rb) = tokio::join!(
        tokio::spawn(async move { mark_completed(&pa, id_a, worker_a, None).await }),
        tokio::spawn(async move { mark_completed(&pb, id_b, worker_b, None).await }),
    );
    ra.unwrap().unwrap();
    rb.unwrap().unwrap();

    let (state, completed): (String, i32) =
        sqlx::query_as("SELECT state, completed FROM eddyq_batches WHERE id = $1")
            .bind(result.batch_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state, "complete");
    assert_eq!(completed, 2);

    let (callback_count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM eddyq_jobs WHERE unique_key = $1",
    )
    .bind(format!("eddyq.batch.{}.callback", result.batch_id))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(callback_count, 1, "exactly one callback under concurrent settle");
}

#[sqlx::test(migrations = "./migrations")]
async fn batch_sweep_stale_settles(pool: PgPool) {
    // Build a batch of 2 items + a callback. Then directly mutate the rows to
    // look like in-flight workers whose heartbeats died — and whose attempts
    // already hit max_attempts so the next sweep marks them terminal-failed.
    let items: Vec<DynEnqueue> = (0..2u64)
        .map(|n| {
            let mut d = item("batch.item", n);
            d.max_attempts = 1;
            d
        })
        .collect();
    let on_complete = DynEnqueue::new("batch.done", serde_json::json!({}));
    let result = enqueue_batch(
        &pool,
        items,
        BatchOptions {
            on_complete: Some(on_complete),
            metadata: serde_json::Value::Null,
        },
    )
    .await
    .unwrap();
    assert_eq!(result.inserted, 2);

    sqlx::query(
        r#"
        UPDATE eddyq_jobs
           SET state        = 'running',
               attempt      = 1,
               heartbeat_at = NOW() - INTERVAL '10 seconds',
               worker_id    = gen_random_uuid()
         WHERE batch_id = $1
        "#,
    )
    .bind(result.batch_id)
    .execute(&pool)
    .await
    .unwrap();

    let recovered = sweep_stale(&pool, Duration::from_secs(1)).await.unwrap();
    assert_eq!(recovered, 2);

    let (state, completed, failed, cancelled): (String, i32, i32, i32) = sqlx::query_as(
        "SELECT state, completed, failed, cancelled FROM eddyq_batches WHERE id = $1",
    )
    .bind(result.batch_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(state, "complete");
    assert_eq!(completed, 0);
    assert_eq!(failed, 2);
    assert_eq!(cancelled, 0);

    let (callback_count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM eddyq_jobs WHERE unique_key = $1",
    )
    .bind(format!("eddyq.batch.{}.callback", result.batch_id))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(callback_count, 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn batch_cancelled_counts_as_terminal(pool: PgPool) {
    let counter = Arc::new(AtomicUsize::new(0));
    let fired = Arc::new(AtomicUsize::new(0));
    let queue = Queue::builder(pool.clone())
        .register::<BatchItem, _>(BatchItemWorker {
            counter: counter.clone(),
            fail_n: None,
        })
        .register::<BatchDone, _>(BatchDoneWorker {
            fired: fired.clone(),
        })
        .config(fast_config())
        .build();

    // 4 items; we'll cancel the first 2 before starting the queue, then run.
    let items: Vec<DynEnqueue> = (0..4u64).map(|n| item("batch.item", n)).collect();
    let on_complete = DynEnqueue::new("batch.done", serde_json::json!({}));
    let result = enqueue_batch(
        &pool,
        items,
        BatchOptions {
            on_complete: Some(on_complete),
            metadata: serde_json::Value::Null,
        },
    )
    .await
    .unwrap();
    assert_eq!(result.inserted, 4);

    let job_ids: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM eddyq_jobs WHERE batch_id = $1 ORDER BY id ASC",
    )
    .bind(result.batch_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(job_ids.len(), 4);
    for id in &job_ids[..2] {
        assert!(queue.cancel(*id).await.unwrap());
    }

    queue.start().unwrap();
    poll_until("2 successes + callback fires", || async {
        fired.load(Ordering::SeqCst) >= 1
    })
    .await;
    queue.shutdown().await.unwrap();

    let (state, completed, failed, cancelled): (String, i32, i32, i32) = sqlx::query_as(
        "SELECT state, completed, failed, cancelled FROM eddyq_batches WHERE id = $1",
    )
    .bind(result.batch_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(state, "complete");
    assert_eq!(completed, 2);
    assert_eq!(cancelled, 2);
    assert_eq!(failed, 0);
    assert_eq!(fired.load(Ordering::SeqCst), 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn batch_mixed_success_failure_still_fires_on_complete(pool: PgPool) {
    let counter = Arc::new(AtomicUsize::new(0));
    let fired = Arc::new(AtomicUsize::new(0));
    let queue = Queue::builder(pool.clone())
        .register::<BatchItem, _>(BatchItemWorker {
            counter: counter.clone(),
            // n=4 fails — and with max_attempts=1 it goes terminal immediately.
            fail_n: Some(4),
        })
        .register::<BatchDone, _>(BatchDoneWorker {
            fired: fired.clone(),
        })
        .config(fast_config())
        .build();

    let items: Vec<DynEnqueue> = (0..5u64)
        .map(|n| {
            let mut d = item("batch.item", n);
            d.max_attempts = 1;
            d
        })
        .collect();
    let on_complete = DynEnqueue::new("batch.done", serde_json::json!({}));
    let result = enqueue_batch(
        &pool,
        items,
        BatchOptions {
            on_complete: Some(on_complete),
            metadata: serde_json::Value::Null,
        },
    )
    .await
    .unwrap();
    assert_eq!(result.inserted, 5);

    queue.start().unwrap();
    poll_until("4 completes + 1 failure + callback fire", || async {
        // 4 successful items + 1 callback fire (callback success increments fired).
        counter.load(Ordering::SeqCst) >= 4 && fired.load(Ordering::SeqCst) >= 1
    })
    .await;
    queue.shutdown().await.unwrap();

    let (state, completed, failed): (String, i32, i32) =
        sqlx::query_as("SELECT state, completed, failed FROM eddyq_batches WHERE id = $1")
            .bind(result.batch_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state, "complete");
    assert_eq!(completed, 4);
    assert_eq!(failed, 1);
    assert_eq!(fired.load(Ordering::SeqCst), 1, "callback fires exactly once");

    // Verify the envelope reports the failed count.
    let (payload,): (serde_json::Value,) =
        sqlx::query_as("SELECT payload FROM eddyq_jobs WHERE unique_key = $1")
            .bind(format!("eddyq.batch.{}.callback", result.batch_id))
            .fetch_one(&pool)
            .await
            .unwrap();
    let envelope = payload.get("_eddyq_batch").unwrap();
    assert_eq!(envelope.get("completed").unwrap(), &serde_json::json!(4));
    assert_eq!(envelope.get("failed").unwrap(), &serde_json::json!(1));
}

#[sqlx::test(migrations = "./migrations")]
async fn batch_retry_does_not_increment(pool: PgPool) {
    let succeed_counter = Arc::new(AtomicUsize::new(0));
    let fired = Arc::new(AtomicUsize::new(0));
    let queue = Queue::builder(pool.clone())
        .register::<BatchFlaky, _>(BatchFlakyWorker {
            counter: succeed_counter.clone(),
        })
        .register::<BatchDone, _>(BatchDoneWorker {
            fired: fired.clone(),
        })
        .config(fast_config())
        .build();

    // One job, max_attempts=3, fails twice then succeeds.
    let mut d = DynEnqueue::new("batch.flaky", serde_json::json!({ "fail_until_attempt": 3 }));
    d.max_attempts = 3;
    let on_complete = DynEnqueue::new("batch.done", serde_json::json!({}));
    let result = enqueue_batch(
        &pool,
        vec![d],
        BatchOptions {
            on_complete: Some(on_complete),
            metadata: serde_json::Value::Null,
        },
    )
    .await
    .unwrap();
    assert_eq!(result.inserted, 1);

    queue.start().unwrap();
    poll_until("flaky succeeds and callback fires", || async {
        fired.load(Ordering::SeqCst) >= 1
    })
    .await;
    queue.shutdown().await.unwrap();

    let (completed, failed): (i32, i32) =
        sqlx::query_as("SELECT completed, failed FROM eddyq_batches WHERE id = $1")
            .bind(result.batch_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        failed, 0,
        "retries must NOT increment failed — the job succeeded on attempt 3"
    );
    assert_eq!(completed, 1);
    assert_eq!(succeed_counter.load(Ordering::SeqCst), 1);
}
