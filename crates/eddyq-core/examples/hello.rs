//! End-to-end demo: migrate, register a worker, enqueue jobs, run to completion.
//!
//! Run against the local docker-compose Postgres:
//!   just db-up
//!   DATABASE_URL=postgres://eddyq:eddyq@localhost:5433/eddyq_dev \
//!     cargo run -p eddyq-core --example hello

#![allow(missing_docs)]

use std::time::Duration;

use eddyq_core::{Job, JobContext, JobResult, Queue, Worker, async_trait};
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgPoolOptions;

#[derive(Debug, Serialize, Deserialize)]
struct Greet {
    name: String,
}

impl Job for Greet {
    const KIND: &'static str = "greet";
}

struct GreetWorker;

#[async_trait]
impl Worker<Greet> for GreetWorker {
    async fn perform(&self, job: Greet, ctx: JobContext) -> JobResult {
        tracing::info!(id = ctx.id, attempt = ctx.attempt, "hello, {}!", job.name);
        Ok(())
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "eddyq_core=debug,hello=info,info".into()),
        )
        .init();

    let db_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://eddyq:eddyq@localhost:5433/eddyq_dev".to_owned());

    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&db_url)
        .await?;

    let queue = Queue::builder(pool)
        .register::<Greet, _>(GreetWorker)
        .worker_concurrency(4)
        .fetch_poll_interval(Duration::from_millis(200))
        .build();

    queue.migrate().await?;

    for name in ["ada", "grace", "linus", "margaret", "edsger"] {
        let res = queue.enqueue(&Greet { name: name.into() }).await?;
        tracing::info!(?res, name, "enqueued");
    }

    queue.start()?;

    // Let the workers drain the batch.
    tokio::time::sleep(Duration::from_secs(2)).await;

    let (pending, completed): (i64, i64) = sqlx::query_as(
        "SELECT
           COUNT(*) FILTER (WHERE state IN ('pending','running')),
           COUNT(*) FILTER (WHERE state = 'completed')
         FROM eddyq_jobs",
    )
    .fetch_one(queue.pool())
    .await?;
    tracing::info!(pending, completed, "job state after run");

    queue.shutdown().await?;
    Ok(())
}
