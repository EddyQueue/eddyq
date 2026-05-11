//! End-to-end throughput harness. Runs the same workload against both
//! `RedisBackend` and `PgBackend` and prints jobs/sec for:
//!   1. Single-job enqueue (sequential round-trip)
//!   2. Bulk enqueue (`enqueue_many` of N at a time)
//!   3. End-to-end drain (enqueue → worker handler returns → completed)
//!
//! Run:
//!     DATABASE_URL=postgres://eddyq:eddyq@localhost:5433/eddyq_dev \
//!     REDIS_URL=redis://127.0.0.1:6381 \
//!     cargo run --release -p eddyq-benches --bin throughput
//!
//! Flags (env vars):
//!     BENCH_JOBS         total jobs per phase (default 10_000)
//!     BENCH_BULK_BATCH   items per enqueue_many call (default 200)
//!     BENCH_WORKERS      per-process concurrency for drain phase (default 16)
//!     BENCH_BACKENDS     comma list: "redis,pg" (default both if env vars set)

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use eddyq_core::{
    Job, JobContext, JobResult, Queue, QueueBuilder, QueueConfig, Worker, async_trait,
    backend::{Backend, PgBackend},
};
use eddyq_redis::{RedisBackend, RedisConfig};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

#[derive(Debug, Serialize, Deserialize)]
struct Bench;

impl Job for Bench {
    const KIND: &'static str = "bench";
}

#[derive(Clone)]
struct CountingWorker {
    counter: Arc<AtomicU64>,
}

#[async_trait]
impl Worker<Bench> for CountingWorker {
    async fn perform(&self, _j: Bench, _ctx: JobContext) -> JobResult {
        self.counter.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn drain_config(workers: usize) -> QueueConfig {
    QueueConfig {
        // Aggressive but realistic: tight polling, small heartbeat,
        // big batch — what a production high-throughput worker pool runs.
        fetch_poll_interval: Duration::from_millis(5),
        fetch_cooldown: Duration::from_millis(1),
        fetch_batch_size: 64,
        worker_concurrency: workers,
        heartbeat_interval: Duration::from_millis(500),
        sweep_interval: Duration::from_secs(5),
        stale_after: Duration::from_secs(30),
        scheduler_interval: Duration::from_millis(500),
        cleanup_interval: Duration::from_secs(600),
        ..QueueConfig::default()
    }
}

async fn measure_single_enqueue<B: Backend>(
    queue: &Queue<B>,
    total: usize,
) -> (Duration, Vec<u128>) {
    let mut latencies = Vec::with_capacity(total);
    let started = Instant::now();
    for _ in 0..total {
        let t = Instant::now();
        queue.enqueue(&Bench).await.unwrap();
        latencies.push(t.elapsed().as_micros());
    }
    (started.elapsed(), latencies)
}

async fn measure_bulk_enqueue<B: Backend>(
    queue: &Queue<B>,
    total: usize,
    batch: usize,
) -> Duration {
    let started = Instant::now();
    let mut remaining = total;
    while remaining > 0 {
        let n = remaining.min(batch);
        let jobs: Vec<Bench> = (0..n).map(|_| Bench).collect();
        queue.enqueue_many(&jobs).await.unwrap();
        remaining -= n;
    }
    started.elapsed()
}

async fn measure_drain<B: Backend>(
    queue: Queue<B>,
    total: usize,
    counter: Arc<AtomicU64>,
) -> Duration {
    let started = Instant::now();
    queue.start().unwrap();
    while counter.load(Ordering::Relaxed) < total as u64 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let elapsed = started.elapsed();
    queue.shutdown().await.unwrap();
    elapsed
}

fn p50_p99(mut v: Vec<u128>) -> (u128, u128) {
    v.sort_unstable();
    let p50 = v[v.len() / 2];
    let p99 = v[(v.len() as f64 * 0.99) as usize];
    (p50, p99)
}

fn print_section(label: &str, total: usize, dur: Duration) {
    let per_s = total as f64 / dur.as_secs_f64();
    println!(
        "  {:<22} {:>10.2} ms total → {:>10.0} jobs/sec",
        label,
        dur.as_secs_f64() * 1000.0,
        per_s
    );
}

async fn bench_redis(url: &str, jobs: usize, bulk: usize, workers: usize) -> anyhow::Result<()> {
    println!(
        "\n=== Redis (line=bench-{}, {} jobs, {} workers) ===",
        std::process::id(),
        jobs,
        workers
    );
    let line = format!("bench-redis-{}", std::process::id());
    let cfg = RedisConfig {
        url: url.to_owned(),
        line: line.clone(),
    };
    // Wipe the namespace so prior runs don't skew numbers.
    flush_redis(url, &line).await?;

    // --- Phase 1: single enqueue ---
    let backend = RedisBackend::connect(cfg.clone()).await?;
    let queue: Queue<RedisBackend> = QueueBuilder::with_backend(backend)
        .config(drain_config(workers))
        .line(line.clone())
        .build();
    let (dur, lat) = measure_single_enqueue(&queue, jobs).await;
    let (p50, p99) = p50_p99(lat);
    print_section("single enqueue", jobs, dur);
    println!("    single latency p50={}µs p99={}µs", p50, p99);
    flush_redis(url, &line).await?;

    // --- Phase 2: bulk enqueue ---
    let backend = RedisBackend::connect(cfg.clone()).await?;
    let queue: Queue<RedisBackend> = QueueBuilder::with_backend(backend)
        .config(drain_config(workers))
        .line(line.clone())
        .build();
    let dur = measure_bulk_enqueue(&queue, jobs, bulk).await;
    print_section(&format!("bulk enqueue (batch {})", bulk), jobs, dur);
    flush_redis(url, &line).await?;

    // --- Phase 3: end-to-end drain ---
    let backend = RedisBackend::connect(cfg.clone()).await?;
    let counter = Arc::new(AtomicU64::new(0));
    let queue: Queue<RedisBackend> = QueueBuilder::with_backend(backend)
        .register::<Bench, _>(CountingWorker {
            counter: counter.clone(),
        })
        .config(drain_config(workers))
        .line(line.clone())
        .build();
    // Pre-enqueue (not measured) then time the drain alone.
    measure_bulk_enqueue(&queue, jobs, bulk).await;
    let dur = measure_drain(queue, jobs, counter).await;
    print_section("end-to-end drain", jobs, dur);
    flush_redis(url, &line).await?;
    Ok(())
}

async fn bench_pg(url: &str, jobs: usize, bulk: usize, workers: usize) -> anyhow::Result<()> {
    println!("\n=== Postgres ({} jobs, {} workers) ===", jobs, workers);
    let pool = PgPool::connect(url).await?;
    // Apply any pending migrations so the bench survives schema-drift on a
    // dev DB that's been around for a while. Cheap when up-to-date.
    eddyq_core::migrate::up(&pool, eddyq_core::migrate::DEFAULT_LINE).await?;
    // Fresh schema per run keeps numbers comparable. (Bench owns the DB.)
    sqlx::query("TRUNCATE eddyq_jobs, eddyq_batches RESTART IDENTITY CASCADE")
        .execute(&pool)
        .await
        .ok();

    let backend = PgBackend::new(pool.clone());
    let queue: Queue<PgBackend> = QueueBuilder::with_backend(backend)
        .config(drain_config(workers))
        .build();
    let (dur, lat) = measure_single_enqueue(&queue, jobs).await;
    let (p50, p99) = p50_p99(lat);
    print_section("single enqueue", jobs, dur);
    println!("    single latency p50={}µs p99={}µs", p50, p99);
    sqlx::query("TRUNCATE eddyq_jobs, eddyq_batches RESTART IDENTITY CASCADE")
        .execute(&pool)
        .await
        .ok();

    let backend = PgBackend::new(pool.clone());
    let queue: Queue<PgBackend> = QueueBuilder::with_backend(backend)
        .config(drain_config(workers))
        .build();
    let dur = measure_bulk_enqueue(&queue, jobs, bulk).await;
    print_section(&format!("bulk enqueue (batch {})", bulk), jobs, dur);
    sqlx::query("TRUNCATE eddyq_jobs, eddyq_batches RESTART IDENTITY CASCADE")
        .execute(&pool)
        .await
        .ok();

    let backend = PgBackend::new(pool.clone());
    let counter = Arc::new(AtomicU64::new(0));
    let queue: Queue<PgBackend> = QueueBuilder::with_backend(backend)
        .register::<Bench, _>(CountingWorker {
            counter: counter.clone(),
        })
        .config(drain_config(workers))
        .build();
    measure_bulk_enqueue(&queue, jobs, bulk).await;
    let dur = measure_drain(queue, jobs, counter).await;
    print_section("end-to-end drain", jobs, dur);

    Ok(())
}

async fn flush_redis(url: &str, line: &str) -> anyhow::Result<()> {
    let client = redis::Client::open(url)?;
    let mut conn = client.get_multiplexed_async_connection().await?;
    let pattern = format!("{{{}}}*", line);
    let keys: Vec<String> = redis::cmd("KEYS")
        .arg(pattern)
        .query_async(&mut conn)
        .await
        .unwrap_or_default();
    if !keys.is_empty() {
        let _: () = redis::cmd("DEL").arg(keys).query_async(&mut conn).await?;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let jobs = env_usize("BENCH_JOBS", 10_000);
    let bulk = env_usize("BENCH_BULK_BATCH", 200);
    let workers = env_usize("BENCH_WORKERS", 16);

    let backends = std::env::var("BENCH_BACKENDS").unwrap_or_else(|_| "redis,pg".to_string());
    let want_redis = backends.contains("redis");
    let want_pg = backends.contains("pg");

    println!(
        "eddyq throughput bench: {} jobs, {} workers, bulk batch {}",
        jobs, workers, bulk
    );

    if want_redis {
        match std::env::var("REDIS_URL") {
            Ok(url) => bench_redis(&url, jobs, bulk, workers).await?,
            Err(_) => println!("[skip] redis — REDIS_URL not set"),
        }
    }
    if want_pg {
        match std::env::var("DATABASE_URL") {
            Ok(url) => bench_pg(&url, jobs, bulk, workers).await?,
            Err(_) => println!("[skip] pg — DATABASE_URL not set"),
        }
    }
    println!();
    Ok(())
}
