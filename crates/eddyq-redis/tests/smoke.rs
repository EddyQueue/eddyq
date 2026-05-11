//! Smoke tests for `RedisBackend`'s hot path. Skipped unless `REDIS_URL`
//! is set so `cargo test` stays green in environments without Redis.
//!
//!     REDIS_URL=redis://127.0.0.1:6381 \
//!         cargo test -p eddyq-redis --test smoke -- --test-threads=1

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use eddyq_core::{
    Job, JobContext, JobResult, Queue, QueueBuilder, QueueConfig, Worker, async_trait,
};
use eddyq_redis::{RedisBackend, RedisConfig};
use serde::{Deserialize, Serialize};

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

fn fast_config() -> QueueConfig {
    QueueConfig {
        fetch_poll_interval: Duration::from_millis(50),
        fetch_cooldown: Duration::from_millis(10),
        fetch_batch_size: 16,
        worker_concurrency: 4,
        heartbeat_interval: Duration::from_millis(100),
        sweep_interval: Duration::from_millis(500),
        stale_after: Duration::from_secs(10),
        scheduler_interval: Duration::from_millis(50),
        cleanup_interval: Duration::from_secs(60),
        ..QueueConfig::default()
    }
}

fn redis_url() -> Option<String> {
    std::env::var("REDIS_URL").ok()
}

/// Allocate a fresh `{line}` per test so the dev Redis can host concurrent
/// tests without keyspace overlap. The `eddyq_v1` library is shared.
fn fresh_line(tag: &str) -> String {
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    format!("smoke-{}-{}", tag, &nonce[..8])
}

async fn flush_line(url: &str, line: &str) {
    use redis::AsyncCommands;
    let client = redis::Client::open(url).unwrap();
    let mut conn = client.get_multiplexed_async_connection().await.unwrap();
    // Wipe every key tied to this line's hash-tag prefix. ScanMatch keeps
    // the test isolated from any pre-existing keys on the dev box.
    let pattern = format!("{{{}}}*", line);
    let mut iter: redis::AsyncIter<String> = conn.scan_match::<_, String>(&pattern).await.unwrap();
    let mut to_del: Vec<String> = Vec::new();
    while let Some(k) = futures_util::StreamExt::next(&mut iter).await {
        to_del.push(k);
    }
    drop(iter);
    if !to_del.is_empty() {
        let _: () = conn.del(to_del).await.unwrap();
    }
}

async fn build_queue(url: &str, line: &str, counter: Arc<AtomicUsize>) -> Queue<RedisBackend> {
    let backend = RedisBackend::connect(RedisConfig {
        url: url.to_owned(),
        line: line.to_owned(),
    })
    .await
    .expect("redis backend connect");

    QueueBuilder::with_backend(backend)
        .register::<Count, _>(CountWorker { counter })
        .config(fast_config())
        .line(line.to_owned())
        .build()
}

#[tokio::test]
async fn enqueue_claim_complete_round_trip() {
    let Some(url) = redis_url() else {
        eprintln!("skipping: REDIS_URL not set");
        return;
    };
    let line = fresh_line("hot");
    flush_line(&url, &line).await;

    let counter = Arc::new(AtomicUsize::new(0));
    let queue = build_queue(&url, &line, counter.clone()).await;
    queue.enqueue(&Count { n: 1 }).await.unwrap();
    queue.start().unwrap();

    tokio::time::timeout(Duration::from_secs(3), async {
        while counter.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("job should run within 3s");

    queue.shutdown().await.unwrap();
    flush_line(&url, &line).await;
}

#[tokio::test]
async fn bulk_enqueue_drains() {
    let Some(url) = redis_url() else {
        eprintln!("skipping: REDIS_URL not set");
        return;
    };
    let line = fresh_line("bulk");
    flush_line(&url, &line).await;

    let counter = Arc::new(AtomicUsize::new(0));
    let queue = build_queue(&url, &line, counter.clone()).await;

    let jobs: Vec<Count> = (0..50).map(|n| Count { n }).collect();
    let res = queue.enqueue_many(&jobs).await.unwrap();
    assert_eq!(res.inserted, 50);
    assert_eq!(res.skipped, 0);

    queue.start().unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while counter.load(Ordering::SeqCst) < 50 {
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .expect("all 50 jobs should run within 10s");

    queue.shutdown().await.unwrap();
    flush_line(&url, &line).await;
}

#[tokio::test]
async fn unique_key_dedupes() {
    use eddyq_core::EnqueueOptions;
    let Some(url) = redis_url() else {
        eprintln!("skipping: REDIS_URL not set");
        return;
    };
    let line = fresh_line("uniq");
    flush_line(&url, &line).await;

    let counter = Arc::new(AtomicUsize::new(0));
    let queue = build_queue(&url, &line, counter.clone()).await;

    let mk = || EnqueueOptions {
        unique_key: Some("dup".into()),
        ..Default::default()
    };
    let r1 = queue.enqueue_with(&Count { n: 1 }, mk()).await.unwrap();
    let r2 = queue.enqueue_with(&Count { n: 2 }, mk()).await.unwrap();

    assert!(
        matches!(r1, eddyq_core::EnqueueResult::Inserted(_)),
        "first enqueue inserted"
    );
    assert!(
        matches!(r2, eddyq_core::EnqueueResult::Skipped),
        "second enqueue skipped: got {:?}",
        r2
    );

    flush_line(&url, &line).await;
}

// =========================================================================
// Group admin + claim gating
// =========================================================================

#[derive(Debug, Serialize, Deserialize)]
struct Hold {
    ms: u64,
}

impl Job for Hold {
    const KIND: &'static str = "hold";
    fn group_key(&self) -> Option<String> {
        Some("g".into())
    }
}

#[derive(Clone)]
struct HoldWorker {
    in_flight: Arc<std::sync::atomic::AtomicU32>,
    peak_in_flight: Arc<std::sync::atomic::AtomicU32>,
    finished: Arc<AtomicUsize>,
}

#[async_trait]
impl Worker<Hold> for HoldWorker {
    async fn perform(&self, job: Hold, _ctx: JobContext) -> JobResult {
        use std::sync::atomic::Ordering;
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        // Track the high-water mark so a `max=1` cap can be observed.
        self.peak_in_flight.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(job.ms)).await;
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        self.finished.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn group_concurrency_cap_holds() {
    use std::sync::atomic::Ordering;
    let Some(url) = redis_url() else {
        eprintln!("skipping: REDIS_URL not set");
        return;
    };
    let line = fresh_line("gcap");
    flush_line(&url, &line).await;

    let in_flight = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let peak = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let finished = Arc::new(AtomicUsize::new(0));

    let backend = RedisBackend::connect(RedisConfig {
        url: url.clone(),
        line: line.clone(),
    })
    .await
    .unwrap();

    let queue = QueueBuilder::with_backend(backend)
        .register::<Hold, _>(HoldWorker {
            in_flight: in_flight.clone(),
            peak_in_flight: peak.clone(),
            finished: finished.clone(),
        })
        .config(fast_config())
        .worker_concurrency(8)
        .line(line.clone())
        .build();

    // Cap the group to 1 concurrent run before enqueueing.
    queue.set_group_concurrency("g", 1).await.unwrap();

    // Enqueue six jobs that each sleep 100ms. With a cap of 1, they must
    // serialize — peak in-flight should stay at 1.
    for _ in 0..6 {
        queue.enqueue(&Hold { ms: 100 }).await.unwrap();
    }
    queue.start().unwrap();

    tokio::time::timeout(Duration::from_secs(5), async {
        while finished.load(Ordering::SeqCst) < 6 {
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .expect("six holds should finish within 5s");

    let observed_peak = peak.load(Ordering::SeqCst);
    assert_eq!(observed_peak, 1, "group cap must serialize jobs");

    let g = queue.get_group("g").await.unwrap().expect("group exists");
    assert_eq!(g.max_concurrency, 1);
    assert!(!g.paused);
    assert_eq!(g.running_count, 0, "no jobs running after drain");

    queue.shutdown().await.unwrap();
    flush_line(&url, &line).await;
}

#[tokio::test]
async fn group_paused_blocks_claim() {
    use std::sync::atomic::Ordering;
    let Some(url) = redis_url() else {
        eprintln!("skipping: REDIS_URL not set");
        return;
    };
    let line = fresh_line("gpause");
    flush_line(&url, &line).await;

    let in_flight = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let peak = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let finished = Arc::new(AtomicUsize::new(0));

    let backend = RedisBackend::connect(RedisConfig {
        url: url.clone(),
        line: line.clone(),
    })
    .await
    .unwrap();

    let queue = QueueBuilder::with_backend(backend)
        .register::<Hold, _>(HoldWorker {
            in_flight: in_flight.clone(),
            peak_in_flight: peak.clone(),
            finished: finished.clone(),
        })
        .config(fast_config())
        .line(line.clone())
        .build();

    queue.pause_group("g").await.unwrap();
    for _ in 0..3 {
        queue.enqueue(&Hold { ms: 50 }).await.unwrap();
    }
    queue.start().unwrap();

    // Give the runtime plenty of time to attempt claims; assert nothing ran.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        finished.load(Ordering::SeqCst),
        0,
        "paused group must not run anything"
    );

    queue.resume_group("g").await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while finished.load(Ordering::SeqCst) < 3 {
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .expect("after resume all three should run");

    queue.shutdown().await.unwrap();
    flush_line(&url, &line).await;
}

#[tokio::test]
async fn group_rate_limit_throttles() {
    use std::sync::atomic::Ordering;
    let Some(url) = redis_url() else {
        eprintln!("skipping: REDIS_URL not set");
        return;
    };
    let line = fresh_line("grate");
    flush_line(&url, &line).await;

    let in_flight = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let peak = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let finished = Arc::new(AtomicUsize::new(0));

    let backend = RedisBackend::connect(RedisConfig {
        url: url.clone(),
        line: line.clone(),
    })
    .await
    .unwrap();

    let queue = QueueBuilder::with_backend(backend)
        .register::<Hold, _>(HoldWorker {
            in_flight: in_flight.clone(),
            peak_in_flight: peak.clone(),
            finished: finished.clone(),
        })
        .config(fast_config())
        .worker_concurrency(16)
        .line(line.clone())
        .build();

    // Allow up to 2 starts per 500ms. Five jobs that each finish in 30ms.
    // The first burst grants 2 immediately, then the next 2 should be gated
    // until refill (~250ms each), and the 5th later still. So the run wall
    // time should be at least ~750ms (3 refills × 250ms).
    queue
        .set_group_rate("g", 2, Duration::from_millis(500))
        .await
        .unwrap();

    let started = std::time::Instant::now();
    for _ in 0..5 {
        queue.enqueue(&Hold { ms: 30 }).await.unwrap();
    }
    queue.start().unwrap();

    tokio::time::timeout(Duration::from_secs(5), async {
        while finished.load(Ordering::SeqCst) < 5 {
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .expect("five rate-limited jobs should finish within 5s");

    let elapsed = started.elapsed();
    // With 2 tokens/500ms, draining 5 jobs needs at least ~750ms (3 refills
    // beyond the initial 2). Use 600ms as a safe lower bound against jitter.
    assert!(
        elapsed >= Duration::from_millis(600),
        "rate limit should have stretched the run: {elapsed:?}"
    );

    queue.shutdown().await.unwrap();
    flush_line(&url, &line).await;
}

// =========================================================================
// Schedules (cron) — fire via leader scheduler loop
// =========================================================================

// =========================================================================
// Named-queue admin: cross-process concurrency cap + pause
// =========================================================================

#[tokio::test]
async fn named_queue_concurrency_cap() {
    use std::sync::atomic::Ordering;
    let Some(url) = redis_url() else {
        eprintln!("skipping: REDIS_URL not set");
        return;
    };
    let line = fresh_line("nqcap");
    flush_line(&url, &line).await;

    let in_flight = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let peak = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let finished = Arc::new(AtomicUsize::new(0));

    let backend = RedisBackend::connect(RedisConfig {
        url: url.clone(),
        line: line.clone(),
    })
    .await
    .unwrap();

    // Hold worker is the same group=g shape; we'll explicitly route to a
    // named queue and cap that queue. Note the worker subscribes to one
    // queue but the in-process concurrency is 8 — only the queue cap should
    // hold the line at 1.
    #[derive(Debug, Serialize, Deserialize)]
    struct NQHold;
    impl Job for NQHold {
        const KIND: &'static str = "nqhold";
        fn queue(&self) -> &'static str {
            "metered"
        }
    }
    #[derive(Clone)]
    struct NQHoldWorker {
        in_flight: Arc<std::sync::atomic::AtomicU32>,
        peak: Arc<std::sync::atomic::AtomicU32>,
        finished: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl Worker<NQHold> for NQHoldWorker {
        async fn perform(&self, _j: NQHold, _ctx: JobContext) -> JobResult {
            let n = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(n, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(80)).await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            self.finished.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    let queue = QueueBuilder::with_backend(backend)
        .register::<NQHold, _>(NQHoldWorker {
            in_flight: in_flight.clone(),
            peak: peak.clone(),
            finished: finished.clone(),
        })
        .config(fast_config())
        .worker_concurrency(8)
        .subscribe_to(["metered"])
        .line(line.clone())
        .build();

    queue.set_queue_concurrency("metered", 1).await.unwrap();
    for _ in 0..4 {
        queue.enqueue(&NQHold).await.unwrap();
    }
    queue.start().unwrap();

    tokio::time::timeout(Duration::from_secs(5), async {
        while finished.load(Ordering::SeqCst) < 4 {
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .expect("queue-capped jobs should drain within 5s");

    assert_eq!(peak.load(Ordering::SeqCst), 1, "queue cap must serialize");

    let nq = queue
        .get_queue("metered")
        .await
        .unwrap()
        .expect("queue registered");
    assert_eq!(nq.max_concurrency, 1);
    assert!(!nq.paused);
    assert_eq!(nq.running_count, 0, "no jobs running after drain");

    queue.shutdown().await.unwrap();
    flush_line(&url, &line).await;
}

#[tokio::test]
async fn schedule_fires_recurring_job() {
    use std::sync::atomic::Ordering;
    let Some(url) = redis_url() else {
        eprintln!("skipping: REDIS_URL not set");
        return;
    };
    let line = fresh_line("sched");
    flush_line(&url, &line).await;

    let counter = Arc::new(AtomicUsize::new(0));
    let queue = build_queue(&url, &line, counter.clone()).await;

    // Add a schedule that fires every second. The runtime's scheduler loop
    // runs at fast_config's 50ms cadence, leader-gated. Within ~2s we should
    // see at least one fire.
    queue
        .add_schedule("count-every-sec", "*/1 * * * * *", &Count { n: 1 })
        .await
        .unwrap();

    queue.start().unwrap();

    tokio::time::timeout(Duration::from_secs(5), async {
        while counter.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("schedule should fire at least once within 5s");

    queue.shutdown().await.unwrap();

    // Listing should surface the schedule with its cron expression.
    let backend = RedisBackend::connect(RedisConfig {
        url: url.clone(),
        line: line.clone(),
    })
    .await
    .unwrap();
    let admin: Queue<RedisBackend> = QueueBuilder::with_backend(backend)
        .config(fast_config())
        .line(line.clone())
        .build();
    let list = admin.list_schedules().await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].name, "count-every-sec");
    assert_eq!(list[0].cron_expr, "*/1 * * * * *");

    // Remove tears down the index.
    let removed = admin.remove_schedule("count-every-sec").await.unwrap();
    assert!(removed);
    let list = admin.list_schedules().await.unwrap();
    assert!(list.is_empty(), "schedule removed");

    flush_line(&url, &line).await;
}

#[tokio::test]
async fn interval_schedule_fires_repeatedly() {
    use std::sync::atomic::Ordering;
    let Some(url) = redis_url() else {
        eprintln!("skipping: REDIS_URL not set");
        return;
    };
    let line = fresh_line("ival");
    flush_line(&url, &line).await;

    let counter = Arc::new(AtomicUsize::new(0));
    let backend = RedisBackend::connect(RedisConfig {
        url: url.clone(),
        line: line.clone(),
    })
    .await
    .unwrap();
    let queue = QueueBuilder::with_backend(backend.clone())
        .register::<Count, _>(CountWorker {
            counter: counter.clone(),
        })
        .config(fast_config())
        .line(line.clone())
        .build();

    // 150ms interval — leader's scheduler loop ticks at 50ms in fast_config,
    // so we should see at least 3 fires in a 600ms window.
    backend
        .upsert_interval_schedule_raw(
            "tick",
            150,
            "count",
            serde_json::json!({ "n": 1 }),
            0,
            3,
            "default",
        )
        .await
        .unwrap();

    queue.start().unwrap();
    tokio::time::sleep(Duration::from_millis(700)).await;
    let fired = counter.load(Ordering::SeqCst);
    queue.shutdown().await.unwrap();

    // Be generous on the upper bound — leader-election races + scheduler
    // tick alignment add a small window. Just assert it kept firing.
    assert!(
        fired >= 3,
        "expected ≥3 fires in 700ms with 150ms interval, got {fired}"
    );

    flush_line(&url, &line).await;
}

#[tokio::test]
async fn schedule_sync_reconciles() {
    use eddyq_core::schedule::ScheduleDeclaration;
    let Some(url) = redis_url() else {
        eprintln!("skipping: REDIS_URL not set");
        return;
    };
    let line = fresh_line("ssync");
    flush_line(&url, &line).await;

    let backend = RedisBackend::connect(RedisConfig {
        url: url.clone(),
        line: line.clone(),
    })
    .await
    .unwrap();
    let queue: Queue<RedisBackend> = QueueBuilder::with_backend(backend)
        .config(fast_config())
        .line(line.clone())
        .build();

    let decls = vec![
        ScheduleDeclaration {
            name: "a".into(),
            cron_expr: "0 * * * * *".into(),
            kind: "count".into(),
            payload: serde_json::json!({ "n": 1 }),
            priority: 0,
            max_attempts: 3,
            queue: "default".into(),
        },
        ScheduleDeclaration {
            name: "b".into(),
            cron_expr: "0 0 * * * *".into(),
            kind: "count".into(),
            payload: serde_json::json!({ "n": 2 }),
            priority: 0,
            max_attempts: 3,
            queue: "default".into(),
        },
    ];
    let report = queue.sync_schedules(&decls).await.unwrap();
    assert_eq!(report.upserted, 2);
    assert!(report.deleted.is_empty());

    // Re-sync without "a" — it should be reported as deleted and disappear.
    let report = queue.sync_schedules(&decls[1..]).await.unwrap();
    assert_eq!(report.upserted, 1);
    assert_eq!(report.deleted, vec!["a".to_string()]);
    let list = queue.list_schedules().await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].name, "b");

    flush_line(&url, &line).await;
}

// =========================================================================
// Group rules — pattern-based auto-materialization on enqueue
// =========================================================================

#[tokio::test]
async fn group_rule_auto_caps_new_keys() {
    use eddyq_core::group::GroupRule;
    use std::sync::atomic::Ordering;
    let Some(url) = redis_url() else {
        eprintln!("skipping: REDIS_URL not set");
        return;
    };
    let line = fresh_line("grule");
    flush_line(&url, &line).await;

    let in_flight = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let peak = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let finished = Arc::new(AtomicUsize::new(0));

    #[derive(Debug, Serialize, Deserialize)]
    struct RuleHold {
        tenant: String,
    }
    impl Job for RuleHold {
        const KIND: &'static str = "rulehold";
        fn group_key(&self) -> Option<String> {
            Some(format!("tenant-{}", self.tenant))
        }
    }
    #[derive(Clone)]
    struct W {
        in_flight: Arc<std::sync::atomic::AtomicU32>,
        peak: Arc<std::sync::atomic::AtomicU32>,
        finished: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl Worker<RuleHold> for W {
        async fn perform(&self, _j: RuleHold, _ctx: JobContext) -> JobResult {
            let n = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(n, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(60)).await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            self.finished.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    let backend = RedisBackend::connect(RedisConfig {
        url: url.clone(),
        line: line.clone(),
    })
    .await
    .unwrap();
    let queue = QueueBuilder::with_backend(backend)
        .register::<RuleHold, _>(W {
            in_flight: in_flight.clone(),
            peak: peak.clone(),
            finished: finished.clone(),
        })
        .config(fast_config())
        .worker_concurrency(8)
        .line(line.clone())
        .build();

    // Register a rule: anything matching tenant-* gets concurrency=1. Then
    // enqueue 4 jobs for tenant-foo — no admin call for that specific key.
    queue
        .set_group_rule("tenant-*", GroupRule::concurrency(1))
        .await
        .unwrap();
    for _ in 0..4 {
        queue
            .enqueue(&RuleHold {
                tenant: "foo".into(),
            })
            .await
            .unwrap();
    }
    queue.start().unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while finished.load(Ordering::SeqCst) < 4 {
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .expect("4 holds drain in 5s");

    assert_eq!(
        peak.load(Ordering::SeqCst),
        1,
        "tenant-foo should inherit concurrency=1 from the tenant-* rule"
    );

    // list_group_rules + remove_group_rule round-trip.
    let rules = queue.list_group_rules().await.unwrap();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].pattern, "tenant-*");
    let removed = queue.remove_group_rule("tenant-*").await.unwrap();
    assert!(removed);
    let rules = queue.list_group_rules().await.unwrap();
    assert!(rules.is_empty());

    queue.shutdown().await.unwrap();
    flush_line(&url, &line).await;
}

// =========================================================================
// Stats + list_jobs (dashboard surface)
// =========================================================================

#[tokio::test]
async fn stats_and_list_jobs_round_trip() {
    use eddyq_core::stats::{ListJobsFilter, Pagination};
    use std::sync::atomic::Ordering;
    let Some(url) = redis_url() else {
        eprintln!("skipping: REDIS_URL not set");
        return;
    };
    let line = fresh_line("stats");
    flush_line(&url, &line).await;

    let counter = Arc::new(AtomicUsize::new(0));
    let queue = build_queue(&url, &line, counter.clone()).await;

    // 5 enqueued, none run yet → all should be pending on the default queue.
    for _ in 0..5 {
        queue.enqueue(&Count { n: 1 }).await.unwrap();
    }
    let stats = queue.get_stats().await.unwrap();
    let pending = stats
        .by_queue_state
        .iter()
        .find(|s| s.queue == "default" && matches!(s.state, eddyq_core::JobState::Pending))
        .expect("expected pending count for default queue");
    assert_eq!(pending.count, 5, "five pending jobs");

    // list_jobs(state=pending) → all 5 ids
    let list = queue
        .list_jobs(
            ListJobsFilter {
                state: Some(eddyq_core::JobState::Pending),
                ..Default::default()
            },
            Pagination::default(),
        )
        .await
        .unwrap();
    assert_eq!(list.total, 5);
    assert_eq!(list.rows.len(), 5);
    for row in &list.rows {
        assert_eq!(row.state, "pending");
        assert_eq!(row.kind, "count");
        assert_eq!(row.queue, "default");
    }

    // Drain them all, then assert completed appears in global stats.
    queue.start().unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while counter.load(Ordering::SeqCst) < 5 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("five count jobs drain in 3s");
    queue.shutdown().await.unwrap();

    let backend = RedisBackend::connect(RedisConfig {
        url: url.clone(),
        line: line.clone(),
    })
    .await
    .unwrap();
    let admin: Queue<RedisBackend> = QueueBuilder::with_backend(backend)
        .config(fast_config())
        .line(line.clone())
        .build();
    let stats = admin.get_stats().await.unwrap();
    let completed = stats
        .by_queue_state
        .iter()
        .find(|s| matches!(s.state, eddyq_core::JobState::Completed))
        .expect("expected completed count");
    assert!(completed.count >= 5, "at least 5 completed");

    let list = admin
        .list_jobs(
            ListJobsFilter {
                state: Some(eddyq_core::JobState::Completed),
                ..Default::default()
            },
            Pagination {
                limit: 3,
                offset: 0,
            },
        )
        .await
        .unwrap();
    assert!(list.total >= 5);
    assert_eq!(list.rows.len(), 3, "pagination caps to limit=3");

    flush_line(&url, &line).await;
}

#[tokio::test]
async fn group_list_round_trips() {
    let Some(url) = redis_url() else {
        eprintln!("skipping: REDIS_URL not set");
        return;
    };
    let line = fresh_line("glist");
    flush_line(&url, &line).await;

    let backend = RedisBackend::connect(RedisConfig {
        url: url.clone(),
        line: line.clone(),
    })
    .await
    .unwrap();
    let queue: Queue<RedisBackend> = QueueBuilder::with_backend(backend)
        .config(fast_config())
        .line(line.clone())
        .build();

    queue.set_group_concurrency("alpha", 4).await.unwrap();
    queue.set_group_concurrency("beta", 2).await.unwrap();
    queue.pause_group("beta").await.unwrap();

    let mut listed = queue.list_groups().await.unwrap();
    listed.sort_by(|a, b| a.key.cmp(&b.key));
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].key, "alpha");
    assert_eq!(listed[0].max_concurrency, 4);
    assert!(!listed[0].paused);
    assert_eq!(listed[1].key, "beta");
    assert_eq!(listed[1].max_concurrency, 2);
    assert!(listed[1].paused);

    flush_line(&url, &line).await;
}
