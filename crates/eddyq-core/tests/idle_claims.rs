//! A process claims only what its idle workers can start. These cover what that must
//! not break: slots always come back, other processes get the work a busy one left,
//! concurrency holds, every job runs once, and shutdown loses nothing. Require a running
//! Postgres at `DATABASE_URL`, like integration.rs.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use eddyq_core::{
    Job, JobContext, JobResult, Queue, QueueConfig, ShutdownMode, Worker, async_trait,
};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

fn config(worker_concurrency: usize) -> QueueConfig {
    QueueConfig {
        fetch_poll_interval: Duration::from_millis(50),
        fetch_cooldown: Duration::from_millis(10),
        fetch_batch_size: 10,
        worker_concurrency,
        heartbeat_interval: Duration::from_millis(100),
        sweep_interval: Duration::from_millis(200),
        // Short enough that a job waiting behind others would look dead if nothing
        // heartbeated it, long enough for a loaded test machine.
        stale_after: Duration::from_secs(2),
        retry_base: Duration::from_millis(20),
        retry_max: Duration::from_millis(100),
        scheduler_interval: Duration::from_millis(100),
        poll_only: false,
        ..Default::default()
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Probe {
    n: u64,
    ms: u64,
}

impl Job for Probe {
    const KIND: &'static str = "probe";
    fn max_attempts(&self) -> i32 {
        3
    }
}

/// What every process's workers saw, shared across processes in one test.
#[derive(Default)]
struct Seen {
    runs: Mutex<HashMap<u64, usize>>,
}

#[derive(Clone)]
struct ProbeWorker {
    seen: Arc<Seen>,
    running: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}

impl ProbeWorker {
    fn new(seen: &Arc<Seen>) -> Self {
        Self {
            seen: seen.clone(),
            running: Arc::default(),
            peak: Arc::default(),
        }
    }
}

#[async_trait]
impl Worker<Probe> for ProbeWorker {
    async fn perform(&self, job: Probe, _ctx: JobContext) -> JobResult {
        *self.seen.runs.lock().unwrap().entry(job.n).or_default() += 1;
        let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(job.ms)).await;
        self.running.fetch_sub(1, Ordering::SeqCst);
        Ok(())
    }
}

async fn states(pool: &PgPool) -> HashMap<String, i64> {
    sqlx::query_as::<_, (String, i64)>("SELECT state, COUNT(*) FROM eddyq_jobs GROUP BY state")
        .fetch_all(pool)
        .await
        .unwrap()
        .into_iter()
        .collect()
}

async fn stalled_total(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT COALESCE(SUM(stalled_count), 0)::bigint FROM eddyq_jobs")
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn wait_all_finished(pool: &PgPool, within: Duration) {
    tokio::time::timeout(within, async {
        loop {
            let s = states(pool).await;
            if s.get("pending").copied().unwrap_or(0) == 0
                && s.get("running").copied().unwrap_or(0) == 0
                && s.get("scheduled").copied().unwrap_or(0) == 0
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("jobs should finish");
}

#[derive(Debug, Serialize, Deserialize)]
struct Failing {
    how: String,
}

impl Job for Failing {
    const KIND: &'static str = "failing";
    fn max_attempts(&self) -> i32 {
        1
    }
}

struct FailingWorker;

#[async_trait]
impl Worker<Failing> for FailingWorker {
    async fn perform(&self, job: Failing, _ctx: JobContext) -> JobResult {
        match job.how.as_str() {
            "panic" => panic!("explicit panic"),
            "error" => Err(anyhow::anyhow!("explicit failure")),
            _ => {
                tokio::time::sleep(Duration::from_secs(30)).await;
                Ok(())
            }
        }
    }
}

/// With one worker, a leaked slot would stop the process claiming for good. Every way a
/// job can end has to hand the slot back.
#[sqlx::test(migrations = "./migrations")]
async fn a_slot_comes_back_however_a_job_ends(pool: PgPool) {
    let seen = Arc::new(Seen::default());
    let queue = Queue::builder(pool.clone())
        .register::<Failing, _>(FailingWorker)
        .register::<Probe, _>(ProbeWorker::new(&seen))
        .config(QueueConfig {
            default_job_timeout: Some(Duration::from_millis(200)),
            ..config(1)
        })
        .build();

    for how in ["panic", "error", "hang"] {
        for _ in 0..3 {
            queue.enqueue(&Failing { how: how.into() }).await.unwrap();
        }
    }
    for n in 0..5 {
        queue.enqueue(&Probe { n, ms: 1 }).await.unwrap();
    }
    queue.start().unwrap();

    wait_all_finished(&pool, Duration::from_secs(20)).await;
    queue.shutdown().await.unwrap();

    let s = states(&pool).await;
    assert_eq!(s.get("failed").copied(), Some(9), "{s:?}");
    assert_eq!(s.get("completed").copied(), Some(5), "{s:?}");
    assert_eq!(seen.runs.lock().unwrap().len(), 5);
}

/// The point of claiming only for idle workers: what a busy process has not started is
/// still there for another process.
#[sqlx::test(migrations = "./migrations")]
async fn another_process_takes_what_a_busy_one_left(pool: PgPool) {
    let seen = Arc::new(Seen::default());
    let busy = Queue::builder(pool.clone())
        .register::<Probe, _>(ProbeWorker::new(&seen))
        .config(config(1))
        .build();

    busy.enqueue(&Probe { n: 0, ms: 3000 }).await.unwrap();
    for n in 1..=4 {
        busy.enqueue(&Probe { n, ms: 10 }).await.unwrap();
    }
    busy.start().unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(seen.runs.lock().unwrap().len(), 1, "only one job started");

    let other = Queue::builder(pool.clone())
        .register::<Probe, _>(ProbeWorker::new(&seen))
        .config(config(1))
        .build();
    other.start().unwrap();

    let started = Instant::now();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if seen.runs.lock().unwrap().len() == 5 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the idle process should run the waiting jobs while the first still works");
    assert!(started.elapsed() < Duration::from_secs(2));

    wait_all_finished(&pool, Duration::from_secs(10)).await;
    other.shutdown().await.unwrap();
    busy.shutdown().await.unwrap();
    assert!(seen.runs.lock().unwrap().values().all(|runs| *runs == 1));
    assert_eq!(stalled_total(&pool).await, 0);
}

/// Mixed short and long jobs across two processes, including jobs longer than
/// `stale_after`: nothing runs twice, nothing looks dead, and neither process goes over
/// its concurrency, by its own count and by the rows it holds in the database.
#[sqlx::test(migrations = "./migrations")]
async fn mixed_load_runs_each_job_once_within_concurrency(pool: PgPool) {
    const JOBS: u64 = 300;
    let seen = Arc::new(Seen::default());
    let first = ProbeWorker::new(&seen);
    let second = ProbeWorker::new(&seen);
    let queue_a = Queue::builder(pool.clone())
        .register::<Probe, _>(first.clone())
        .config(config(4))
        .build();
    let queue_b = Queue::builder(pool.clone())
        .register::<Probe, _>(second.clone())
        .config(config(2))
        .build();

    let mut state = 7u64;
    for n in 0..JOBS {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        let ms = match (state >> 33) % 100 {
            0..=69 => (state >> 40) % 10,
            70..=97 => 20 + (state >> 40) % 80,
            _ => 2_500,
        };
        queue_a.enqueue(&Probe { n, ms }).await.unwrap();
    }

    queue_a.start().unwrap();
    queue_b.start().unwrap();

    let peak_rows = Arc::new(AtomicUsize::new(0));
    let peak_rows_writer = peak_rows.clone();
    let sampler_pool = pool.clone();
    let row_sampler = tokio::spawn(async move {
        loop {
            let running: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM eddyq_jobs WHERE state = 'running'")
                    .fetch_one(&sampler_pool)
                    .await
                    .unwrap();
            peak_rows_writer.fetch_max(running as usize, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });

    wait_all_finished(&pool, Duration::from_secs(60)).await;
    row_sampler.abort();
    queue_a.shutdown().await.unwrap();
    queue_b.shutdown().await.unwrap();

    let runs = seen.runs.lock().unwrap().clone();
    assert_eq!(runs.len() as u64, JOBS, "every job ran");
    let repeated: Vec<_> = runs.iter().filter(|(_, count)| **count != 1).collect();
    assert!(
        repeated.is_empty(),
        "jobs that ran more than once: {repeated:?}"
    );
    assert_eq!(
        stalled_total(&pool).await,
        0,
        "no job was recovered as stalled"
    );

    assert!(first.peak.load(Ordering::SeqCst) <= 4);
    assert!(second.peak.load(Ordering::SeqCst) <= 2);
    assert!(
        peak_rows.load(Ordering::SeqCst) <= 6,
        "running rows reached {}, over the 6 workers that exist",
        peak_rows.load(Ordering::SeqCst)
    );
    assert_eq!(
        states(&pool).await.get("completed").copied(),
        Some(JOBS as i64)
    );
}

/// A graceful stop under load leaves nothing stuck: what it had not started goes to the
/// next process, and nothing runs twice.
#[sqlx::test(migrations = "./migrations")]
async fn drain_under_load_loses_nothing(pool: PgPool) {
    const JOBS: u64 = 40;
    let seen = Arc::new(Seen::default());
    let first = Queue::builder(pool.clone())
        .register::<Probe, _>(ProbeWorker::new(&seen))
        .config(config(3))
        .build();
    for n in 0..JOBS {
        first.enqueue(&Probe { n, ms: 100 }).await.unwrap();
    }
    first.start().unwrap();
    tokio::time::sleep(Duration::from_millis(350)).await;
    first.shutdown().await.unwrap();

    let next = Queue::builder(pool.clone())
        .register::<Probe, _>(ProbeWorker::new(&seen))
        .config(config(3))
        .build();
    next.start().unwrap();
    wait_all_finished(&pool, Duration::from_secs(30)).await;
    next.shutdown().await.unwrap();

    let runs = seen.runs.lock().unwrap().clone();
    assert_eq!(runs.len() as u64, JOBS);
    assert!(runs.values().all(|count| *count == 1), "{runs:?}");
    assert_eq!(
        states(&pool).await.get("completed").copied(),
        Some(JOBS as i64)
    );
}

/// A forced stop may repeat what it interrupted, which is its contract, but must not lose
/// anything or leave a row running.
#[sqlx::test(migrations = "./migrations")]
async fn force_under_load_loses_nothing(pool: PgPool) {
    const JOBS: u64 = 40;
    let seen = Arc::new(Seen::default());
    let first = Queue::builder(pool.clone())
        .register::<Probe, _>(ProbeWorker::new(&seen))
        .config(config(3))
        .build();
    for n in 0..JOBS {
        first.enqueue(&Probe { n, ms: 100 }).await.unwrap();
    }
    first.start().unwrap();
    tokio::time::sleep(Duration::from_millis(350)).await;
    first.shutdown_with(ShutdownMode::Force).await.unwrap();

    assert_eq!(
        states(&pool).await.get("running").copied().unwrap_or(0),
        0,
        "force returns everything it held"
    );

    let next = Queue::builder(pool.clone())
        .register::<Probe, _>(ProbeWorker::new(&seen))
        .config(config(3))
        .build();
    next.start().unwrap();
    wait_all_finished(&pool, Duration::from_secs(30)).await;
    next.shutdown().await.unwrap();

    assert_eq!(seen.runs.lock().unwrap().len() as u64, JOBS);
    assert_eq!(
        states(&pool).await.get("completed").copied(),
        Some(JOBS as i64)
    );
}

/// A small worker pool with a large fetch batch must still claim only what it can start.
#[sqlx::test(migrations = "./migrations")]
async fn a_large_batch_size_does_not_over_claim(pool: PgPool) {
    let seen = Arc::new(Seen::default());
    let queue = Queue::builder(pool.clone())
        .register::<Probe, _>(ProbeWorker::new(&seen))
        .config(QueueConfig {
            fetch_batch_size: 100,
            ..config(2)
        })
        .build();
    for n in 0..20 {
        queue.enqueue(&Probe { n, ms: 400 }).await.unwrap();
    }
    queue.start().unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(states(&pool).await.get("running").copied(), Some(2));
    queue.shutdown_with(ShutdownMode::Force).await.unwrap();
}
