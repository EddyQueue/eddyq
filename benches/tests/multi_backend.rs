//! Dual-backend integration test. Requires *both* `DATABASE_URL` and
//! `REDIS_URL`. Lives in the benches crate because that's the only
//! workspace member that depends on `eddyq-core` (PG) + `eddyq-redis`
//! together.
//!
//! Skipped (not failed) when either env var is missing — keeps `cargo
//! test --workspace` green in environments without one of the stacks.
//!
//!     just db-up redis-up
//!     DATABASE_URL=postgres://eddyq:eddyq@localhost:5433/eddyq_dev \
//!     REDIS_URL=redis://127.0.0.1:6381 \
//!         cargo test -p eddyq-benches --test multi_backend -- --test-threads=1

#![allow(clippy::too_many_lines)]

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use eddyq_core::{
    Job, JobContext, JobResult, Queue, QueueBuilder, QueueConfig, Worker, async_trait,
    backend::PgBackend,
};
use eddyq_redis::{RedisBackend, RedisConfig};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

// Each test uses a distinct (kind, queue) tuple so the two tests can run in
// parallel against the same shared dev DB + Redis without contaminating
// each other's counters. The `_b` suffix is the mis-subscription test.
#[derive(Debug, Serialize, Deserialize)]
struct PingPgA;
impl Job for PingPgA {
    const KIND: &'static str = "ping.pg.a";
    fn queue(&self) -> &'static str {
        "payments-a"
    }
}
#[derive(Debug, Serialize, Deserialize)]
struct PingRedisA;
impl Job for PingRedisA {
    const KIND: &'static str = "ping.redis.a";
    fn queue(&self) -> &'static str {
        "webhooks-a"
    }
}
#[derive(Debug, Serialize, Deserialize)]
struct PingPgB;
impl Job for PingPgB {
    const KIND: &'static str = "ping.pg.b";
    fn queue(&self) -> &'static str {
        "payments-b"
    }
}
#[derive(Debug, Serialize, Deserialize)]
struct PingRedisB;
impl Job for PingRedisB {
    const KIND: &'static str = "ping.redis.b";
    fn queue(&self) -> &'static str {
        "webhooks-b"
    }
}

#[derive(Clone)]
struct LaneCounter {
    pg_count: Arc<AtomicU64>,
    redis_count: Arc<AtomicU64>,
}

macro_rules! count_worker {
    ($job:ty, $field:ident) => {
        #[async_trait]
        impl Worker<$job> for LaneCounter {
            async fn perform(&self, _j: $job, _ctx: JobContext) -> JobResult {
                self.$field.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }
    };
}
count_worker!(PingPgA, pg_count);
count_worker!(PingRedisA, redis_count);
count_worker!(PingPgB, pg_count);
count_worker!(PingRedisB, redis_count);

fn fast_cfg() -> QueueConfig {
    QueueConfig {
        fetch_poll_interval: Duration::from_millis(40),
        fetch_cooldown: Duration::from_millis(10),
        fetch_batch_size: 16,
        worker_concurrency: 4,
        heartbeat_interval: Duration::from_millis(200),
        sweep_interval: Duration::from_millis(500),
        stale_after: Duration::from_secs(10),
        scheduler_interval: Duration::from_millis(200),
        cleanup_interval: Duration::from_secs(60),
        ..QueueConfig::default()
    }
}

async fn flush_redis(url: &str, line: &str) {
    let client = redis::Client::open(url).unwrap();
    let mut conn = client.get_multiplexed_async_connection().await.unwrap();
    let pattern = format!("{{{}}}*", line);
    let keys: Vec<String> = redis::cmd("KEYS")
        .arg(pattern)
        .query_async(&mut conn)
        .await
        .unwrap_or_default();
    if !keys.is_empty() {
        let _: () = redis::cmd("DEL")
            .arg(keys)
            .query_async(&mut conn)
            .await
            .unwrap();
    }
}

/// Drives both backends from the same process. Confirms that:
///   1. The Backend trait is identical-shape enough that the runtime works
///      against either with no special casing.
///   2. Jobs enqueued to one backend's queue do NOT bleed to the other —
///      no shared state, no accidental cross-talk.
///   3. The same handler trait + JobContext shape services both.
#[tokio::test]
async fn pg_and_redis_run_side_by_side() {
    let (Ok(pg_url), Ok(redis_url)) = (std::env::var("DATABASE_URL"), std::env::var("REDIS_URL"))
    else {
        eprintln!("skipping: DATABASE_URL and REDIS_URL must both be set");
        return;
    };

    // Per-test Redis namespace, fresh between runs.
    let line = format!("multi-{}", &uuid::Uuid::new_v4().simple().to_string()[..10]);
    flush_redis(&redis_url, &line).await;

    let pg_pool = PgPool::connect(&pg_url).await.unwrap();
    eddyq_core::migrate::up(&pg_pool, eddyq_core::migrate::DEFAULT_LINE)
        .await
        .unwrap();
    // Isolate counts from any leftover rows in the dev DB.
    sqlx::query("DELETE FROM eddyq_jobs WHERE kind IN ('ping.pg.a', 'ping.redis.a')")
        .execute(&pg_pool)
        .await
        .ok();

    let counter = LaneCounter {
        pg_count: Arc::new(AtomicU64::new(0)),
        redis_count: Arc::new(AtomicU64::new(0)),
    };

    // PG queue, subscribed to "payments-a".
    let pg_backend = PgBackend::new(pg_pool.clone());
    let pg_queue: Queue<PgBackend> = QueueBuilder::with_backend(pg_backend)
        .register::<PingPgA, _>(counter.clone())
        .config(fast_cfg())
        .subscribe_to(["payments-a"])
        .build();

    // Redis queue, subscribed to "webhooks-a".
    let redis_backend = RedisBackend::connect(RedisConfig {
        url: redis_url.clone(),
        line: line.clone(),
    })
    .await
    .unwrap();
    let redis_queue: Queue<RedisBackend> = QueueBuilder::with_backend(redis_backend)
        .register::<PingRedisA, _>(counter.clone())
        .config(fast_cfg())
        .subscribe_to(["webhooks-a"])
        .line(line.clone())
        .build();

    // Enqueue to both, in interleaved order, to prove the runtimes don't
    // serialize against each other.
    for _ in 0..5 {
        pg_queue.enqueue(&PingPgA).await.unwrap();
        redis_queue.enqueue(&PingRedisA).await.unwrap();
    }

    pg_queue.start().unwrap();
    redis_queue.start().unwrap();

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let pg = counter.pg_count.load(Ordering::SeqCst);
            let redis = counter.redis_count.load(Ordering::SeqCst);
            if pg >= 5 && redis >= 5 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    })
    .await
    .expect("both backends should drain 5 jobs each within 5s");

    let pg_done = counter.pg_count.load(Ordering::SeqCst);
    let redis_done = counter.redis_count.load(Ordering::SeqCst);

    pg_queue.shutdown().await.unwrap();
    redis_queue.shutdown().await.unwrap();
    flush_redis(&redis_url, &line).await;

    // Sanity: each lane saw exactly its own jobs — no cross-talk.
    assert_eq!(pg_done, 5, "PG lane ran exactly 5 ping.pg jobs");
    assert_eq!(redis_done, 5, "Redis lane ran exactly 5 ping.redis jobs");
}

/// Belt-and-suspenders: subscribing the PG queue to "webhooks" (a queue
/// only the Redis backend should pick up) must NOT cause it to materialize
/// any jobs — the kinds differ, the storage backends differ. The point is
/// that the runtime composition stays isolated even under operator error.
#[tokio::test]
async fn cross_backend_isolation_under_mis_subscription() {
    let (Ok(pg_url), Ok(redis_url)) = (std::env::var("DATABASE_URL"), std::env::var("REDIS_URL"))
    else {
        eprintln!("skipping: DATABASE_URL and REDIS_URL must both be set");
        return;
    };

    let line = format!("isol-{}", &uuid::Uuid::new_v4().simple().to_string()[..10]);
    flush_redis(&redis_url, &line).await;

    let pg_pool = PgPool::connect(&pg_url).await.unwrap();
    eddyq_core::migrate::up(&pg_pool, eddyq_core::migrate::DEFAULT_LINE)
        .await
        .unwrap();
    sqlx::query("DELETE FROM eddyq_jobs WHERE kind IN ('ping.pg.b', 'ping.redis.b')")
        .execute(&pg_pool)
        .await
        .ok();

    let counter = LaneCounter {
        pg_count: Arc::new(AtomicU64::new(0)),
        redis_count: Arc::new(AtomicU64::new(0)),
    };

    // PG queue subscribed to BOTH "payments-b" and "webhooks-b" (the latter
    // is a mistake — but it should be harmless because PG has no Redis jobs).
    let pg_queue: Queue<PgBackend> = QueueBuilder::with_backend(PgBackend::new(pg_pool.clone()))
        .register::<PingPgB, _>(counter.clone())
        .config(fast_cfg())
        .subscribe_to(["payments-b", "webhooks-b"])
        .build();

    let redis_queue: Queue<RedisBackend> = QueueBuilder::with_backend(
        RedisBackend::connect(RedisConfig {
            url: redis_url.clone(),
            line: line.clone(),
        })
        .await
        .unwrap(),
    )
    .register::<PingRedisB, _>(counter.clone())
    .config(fast_cfg())
    .subscribe_to(["webhooks-b"])
    .line(line.clone())
    .build();

    redis_queue.enqueue(&PingRedisB).await.unwrap();
    redis_queue.enqueue(&PingRedisB).await.unwrap();
    pg_queue.enqueue(&PingPgB).await.unwrap();

    pg_queue.start().unwrap();
    redis_queue.start().unwrap();

    tokio::time::sleep(Duration::from_millis(800)).await;

    pg_queue.shutdown().await.unwrap();
    redis_queue.shutdown().await.unwrap();

    let pg_done = counter.pg_count.load(Ordering::SeqCst);
    let redis_done = counter.redis_count.load(Ordering::SeqCst);
    flush_redis(&redis_url, &line).await;

    assert_eq!(pg_done, 1, "PG lane should only run its own job");
    assert_eq!(redis_done, 2, "Redis lane should run its own 2 jobs");
}
