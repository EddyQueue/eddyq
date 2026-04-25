use std::str::FromStr;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use eddyq_core::{
    enqueue::{EnqueueOptions, enqueue, enqueue_many},
    job::Job,
    migrate,
};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use sqlx::postgres::PgConnectOptions;

#[derive(Serialize, Deserialize, Clone)]
struct BenchJob {
    data: String,
}

impl Job for BenchJob {
    const KIND: &'static str = "bench";
}

fn payload(size: usize) -> BenchJob {
    BenchJob {
        data: "x".repeat(size),
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

fn bench_enqueue_single(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let pool = rt.block_on(setup());

    c.bench_function("enqueue/single", |b| {
        b.to_async(&rt).iter(|| {
            let pool = pool.clone();
            async move {
                enqueue(&pool, &payload(256), EnqueueOptions::default())
                    .await
                    .unwrap();
            }
        })
    });
}

fn bench_enqueue_bulk(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let pool = rt.block_on(setup());

    let mut group = c.benchmark_group("enqueue/bulk");
    for n in [10usize, 100, 1_000] {
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            let jobs: Vec<BenchJob> = (0..n).map(|_| payload(256)).collect();
            b.to_async(&rt).iter(|| {
                let pool = pool.clone();
                let jobs = jobs.clone();
                async move {
                    enqueue_many(&pool, &jobs).await.unwrap();
                }
            })
        });
    }
    group.finish();
}

criterion_group!(benches, bench_enqueue_single, bench_enqueue_bulk);
criterion_main!(benches);
