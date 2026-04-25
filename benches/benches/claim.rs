use std::str::FromStr;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use eddyq_core::{enqueue::enqueue_many, fetch::claim_batch, job::Job, migrate};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use sqlx::postgres::PgConnectOptions;
use uuid::Uuid;

#[derive(Serialize, Deserialize, Clone)]
struct BenchJob {
    data: String,
}

impl Job for BenchJob {
    const KIND: &'static str = "bench";
}

fn payload() -> BenchJob {
    BenchJob {
        data: "x".repeat(256),
    }
}

async fn setup() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set to run benchmarks");
    let opts = PgConnectOptions::from_str(&url)
        .expect("invalid DATABASE_URL")
        .options([("search_path", "bench")]);
    let pool = PgPool::connect_with(opts).await.expect("failed to connect");

    // Fresh schema every run — safe to point at any Postgres instance.
    sqlx::query("DROP SCHEMA IF EXISTS bench CASCADE")
        .execute(&pool)
        .await
        .expect("drop schema failed");
    sqlx::query("CREATE SCHEMA bench")
        .execute(&pool)
        .await
        .expect("create schema failed");
    migrate::up(&pool, migrate::DEFAULT_LINE)
        .await
        .expect("migrations failed");
    pool
}

fn bench_claim_batch(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let pool = rt.block_on(setup());
    let worker_id = Uuid::new_v4();
    let kinds = vec!["bench".to_string()];
    let queues = vec!["default".to_string()];

    let mut group = c.benchmark_group("claim_batch");
    for batch_size in [1usize, 10, 50] {
        group.bench_with_input(
            BenchmarkId::from_parameter(batch_size),
            &batch_size,
            |b, &batch_size| {
                b.to_async(&rt).iter_custom(|iters| {
                    let pool = pool.clone();
                    let kinds = kinds.clone();
                    let queues = queues.clone();
                    async move {
                        // Pre-populate enough jobs for all iterations (not timed).
                        let n = (iters as usize) * batch_size;
                        let jobs: Vec<BenchJob> = (0..n).map(|_| payload()).collect();
                        enqueue_many(&pool, &jobs).await.unwrap();

                        let start = std::time::Instant::now();
                        for _ in 0..iters {
                            claim_batch(&pool, worker_id, batch_size, &kinds, &queues)
                                .await
                                .unwrap();
                        }
                        start.elapsed()
                    }
                })
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_claim_batch);
criterion_main!(benches);
