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
    assert_eq!(list[0].cron_expr.as_deref(), Some("*/1 * * * * *"));

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

/// Per-queue stats attribution: completed/failed/cancelled jobs land in
/// `nq:<q>:<state>` mirror ZSETs and surface under their queue in
/// `get_stats`, not under the legacy `_global` bucket. Regression guard
/// for the wakeboard dashboard showing every Redis completion as
/// `_global` because the global ZSETs weren't queue-partitioned.
#[tokio::test]
async fn stats_partition_completed_per_queue() {
    use eddyq_core::enqueue::EnqueueOptions;
    let Some(url) = redis_url() else {
        eprintln!("skipping: REDIS_URL not set");
        return;
    };
    let line = fresh_line("statspq");
    flush_line(&url, &line).await;

    let counter = Arc::new(AtomicUsize::new(0));
    // Workers default to subscribing only to `default`; subscribe to the
    // two named queues so they actually get drained.
    let backend = RedisBackend::connect(RedisConfig {
        url: url.to_owned(),
        line: line.to_owned(),
    })
    .await
    .expect("redis backend connect");
    let queue: Queue<RedisBackend> = QueueBuilder::with_backend(backend)
        .register::<Count, _>(CountWorker {
            counter: counter.clone(),
        })
        .config(fast_config())
        .line(line.to_owned())
        .subscribe_to(["alpha".to_owned(), "beta".to_owned()])
        .build();

    // 3 jobs on queue "alpha", 2 on queue "beta".
    for _ in 0..3 {
        queue
            .enqueue_with(
                &Count { n: 1 },
                EnqueueOptions {
                    queue: Some("alpha".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
    }
    for _ in 0..2 {
        queue
            .enqueue_with(
                &Count { n: 1 },
                EnqueueOptions {
                    queue: Some("beta".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
    }

    queue.start().unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while counter.load(Ordering::SeqCst) < 5 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("five jobs drain in 3s");
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

    let find_completed = |q: &str| -> i64 {
        stats
            .by_queue_state
            .iter()
            .find(|s| s.queue == q && matches!(s.state, eddyq_core::JobState::Completed))
            .map(|s| s.count)
            .unwrap_or(0)
    };
    assert_eq!(find_completed("alpha"), 3, "alpha completed count");
    assert_eq!(find_completed("beta"), 2, "beta completed count");
    // No `_global` row should appear once every completed job has been
    // mirrored into its per-queue ZSET (the remainder calc nets to zero).
    let global = find_completed("_global");
    assert_eq!(global, 0, "no _global remainder after per-queue mirroring");

    flush_line(&url, &line).await;
}

/// Backfill function attributes legacy global-ZSET entries to per-queue
/// mirrors. Simulates the "upgraded from old library" path: we hand-write
/// rows into `:completed` *without* mirroring (bypassing fn_complete),
/// then invoke `eddyq_backfill_nq_states` and assert the mirrors fill in
/// and `_global` empties.
#[tokio::test]
async fn backfill_attributes_legacy_global_entries() {
    let Some(url) = redis_url() else {
        eprintln!("skipping: REDIS_URL not set");
        return;
    };
    let line = fresh_line("backfill");
    flush_line(&url, &line).await;

    // Connect once so the library is loaded.
    let backend = RedisBackend::connect(RedisConfig {
        url: url.clone(),
        line: line.clone(),
    })
    .await
    .unwrap();

    // Hand-write three legacy completed jobs (job hash + global ZSET only,
    // no per-queue mirror) to simulate state from before this upgrade.
    let client = redis::Client::open(url.as_str()).unwrap();
    let mut conn = client.get_multiplexed_async_connection().await.unwrap();
    let prefix = format!("{{{}}}", line);
    let global_completed = format!("{}:completed", prefix);
    for (id, q) in [(1001i64, "alpha"), (1002, "alpha"), (1003, "beta")] {
        let _: () = redis::cmd("HSET")
            .arg(format!("{}:job:{}", prefix, id))
            .arg("queue")
            .arg(q)
            .arg("state")
            .arg("completed")
            .query_async(&mut conn)
            .await
            .unwrap();
        let _: () = redis::cmd("ZADD")
            .arg(&global_completed)
            .arg(1_700_000_000_000i64)
            .arg(id)
            .query_async(&mut conn)
            .await
            .unwrap();
    }

    // Before backfill: `_global` carries the three entries.
    let admin: Queue<RedisBackend> = QueueBuilder::with_backend(backend)
        .config(fast_config())
        .line(line.clone())
        .build();
    let pre = admin.get_stats().await.unwrap();
    let pre_global = pre
        .by_queue_state
        .iter()
        .find(|s| s.queue == "_global" && matches!(s.state, eddyq_core::JobState::Completed))
        .map(|s| s.count)
        .unwrap_or(0);
    assert_eq!(
        pre_global, 3,
        "legacy entries surface as _global before backfill"
    );

    // Run backfill.
    let inserted: redis::Value = redis::cmd("FCALL")
        .arg("eddyq_backfill_nq_states")
        .arg(1)
        .arg(&prefix)
        .query_async(&mut conn)
        .await
        .unwrap();
    let n = match inserted {
        redis::Value::Int(i) => i,
        _ => panic!("expected int reply, got {:?}", inserted),
    };
    assert_eq!(n, 3, "backfill mirrors three entries");

    // After backfill: per-queue counts populated, `_global` is zero.
    let post = admin.get_stats().await.unwrap();
    let find_q = |q: &str| -> i64 {
        post.by_queue_state
            .iter()
            .find(|s| s.queue == q && matches!(s.state, eddyq_core::JobState::Completed))
            .map(|s| s.count)
            .unwrap_or(0)
    };
    assert_eq!(find_q("alpha"), 2, "alpha attribution after backfill");
    assert_eq!(find_q("beta"), 1, "beta attribution after backfill");
    assert_eq!(find_q("_global"), 0, "no _global remainder after backfill");

    // Idempotent: second backfill call inserts nothing.
    let again: redis::Value = redis::cmd("FCALL")
        .arg("eddyq_backfill_nq_states")
        .arg(1)
        .arg(&prefix)
        .query_async(&mut conn)
        .await
        .unwrap();
    let n2 = match again {
        redis::Value::Int(i) => i,
        _ => -1,
    };
    assert_eq!(n2, 0, "backfill is idempotent");

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

// ====================================================================
// Retention & cleanup
// ====================================================================

/// `ZCARD {prefix}:completed` — used by the retention tests to assert how
/// many job IDs remain in the finalized index after a sweep.
async fn zcard_completed(url: &str, line: &str) -> i64 {
    use redis::AsyncCommands;
    let client = redis::Client::open(url).unwrap();
    let mut conn = client.get_multiplexed_async_connection().await.unwrap();
    let key = format!("{{{}}}:completed", line);
    conn.zcard(&key).await.unwrap()
}

/// `ZCARD {prefix}:failed` — parallel to `zcard_completed` for the failed
/// finalized index. Used to assert that count caps on completed don't bleed
/// into the failed ZSET.
async fn zcard_failed(url: &str, line: &str) -> i64 {
    use redis::AsyncCommands;
    let client = redis::Client::open(url).unwrap();
    let mut conn = client.get_multiplexed_async_connection().await.unwrap();
    let key = format!("{{{}}}:failed", line);
    conn.zcard(&key).await.unwrap()
}

/// Drive the queue until `counter` reaches `target`, then shut down. Used by
/// retention tests that need workers to finalize jobs before they assert on
/// the resulting `completed` ZSET.
async fn run_until(queue: &Queue<RedisBackend>, counter: &AtomicUsize, target: usize) {
    queue.start().unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while counter.load(Ordering::SeqCst) < target {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("jobs should finalize within 5s");
    queue.shutdown().await.unwrap();
}

/// Per-job `removeOnComplete: { count: 3 }` keeps only the 3 most recently
/// completed jobs in the finalized ZSET. Inline retention runs in
/// `apply_retention` on each `complete` call — no queue-default sweep needed.
#[tokio::test]
async fn per_job_retention_keep_count_caps_completed_zset() {
    use eddyq_core::{EnqueueOptions, RetentionRule};
    let Some(url) = redis_url() else {
        eprintln!("skipping: REDIS_URL not set");
        return;
    };
    let line = fresh_line("retc");
    flush_line(&url, &line).await;

    let counter = Arc::new(AtomicUsize::new(0));
    let queue = build_queue(&url, &line, counter.clone()).await;

    for n in 0..5 {
        let opts = EnqueueOptions {
            remove_on_complete: Some(RetentionRule::keep_count(3)),
            ..Default::default()
        };
        queue.enqueue_with(&Count { n }, opts).await.unwrap();
    }

    run_until(&queue, &counter, 5).await;

    let remaining = zcard_completed(&url, &line).await;
    assert_eq!(
        remaining, 3,
        "per-job count:3 retention should keep exactly 3 in completed ZSET"
    );

    flush_line(&url, &line).await;
}

/// `removeOnComplete: true` deletes the job HASH and removes it from every
/// index on finalize. The completed ZSET ends up empty even though jobs
/// were processed successfully.
#[tokio::test]
async fn per_job_retention_drop_removes_hash() {
    use eddyq_core::{EnqueueOptions, RetentionRule};
    use redis::AsyncCommands;
    let Some(url) = redis_url() else {
        eprintln!("skipping: REDIS_URL not set");
        return;
    };
    let line = fresh_line("retd");
    flush_line(&url, &line).await;

    let counter = Arc::new(AtomicUsize::new(0));
    let queue = build_queue(&url, &line, counter.clone()).await;

    let opts = EnqueueOptions {
        remove_on_complete: Some(RetentionRule::drop()),
        ..Default::default()
    };
    let eddyq_core::EnqueueResult::Inserted(id) =
        queue.enqueue_with(&Count { n: 1 }, opts).await.unwrap()
    else {
        panic!("expected Inserted");
    };

    run_until(&queue, &counter, 1).await;

    let client = redis::Client::open(url.as_str()).unwrap();
    let mut conn = client.get_multiplexed_async_connection().await.unwrap();
    let jobkey = format!("{{{}}}:job:{}", line, id);
    let exists: i64 = conn.exists(&jobkey).await.unwrap();
    assert_eq!(exists, 0, "drop retention must delete the job HASH");
    let zcard = zcard_completed(&url, &line).await;
    assert_eq!(
        zcard, 0,
        "drop retention must skip ZADD to the completed index"
    );

    flush_line(&url, &line).await;
}

/// Queue-default sweep via `Backend::cleanup` handles jobs that opted out of
/// per-job retention (`false`) — `apply_retention` ZADDs them on finalize,
/// then the leader-driven `cleanup` tick prunes by age. Drives `cleanup`
/// directly here rather than spinning up the leader loop.
#[tokio::test]
async fn queue_default_cleanup_sweeps_completed_by_age() {
    use eddyq_core::backend::Backend;
    use eddyq_core::fetch::Retention;
    let Some(url) = redis_url() else {
        eprintln!("skipping: REDIS_URL not set");
        return;
    };
    let line = fresh_line("retq");
    flush_line(&url, &line).await;

    let counter = Arc::new(AtomicUsize::new(0));
    let queue = build_queue(&url, &line, counter.clone()).await;

    for n in 0..4 {
        queue.enqueue(&Count { n }).await.unwrap();
    }
    run_until(&queue, &counter, 4).await;

    assert_eq!(
        zcard_completed(&url, &line).await,
        4,
        "no per-job rule => all 4 land in completed ZSET"
    );

    // Let the wall clock advance past the cutoff. `completed_secs = 0` means
    // cutoff = now_ms; the ZRANGEBYSCORE is exclusive on the upper bound, so
    // a finalized_at score equal to now would not be swept. The tiny sleep
    // moves now past every score we just wrote.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let backend = queue.backend().as_ref();
    let (c, _, _, _) = backend
        .cleanup(Retention {
            completed_secs: Some(0),
            ..Retention::default()
        })
        .await
        .unwrap();
    assert_eq!(c, 4, "cleanup should sweep all 4 from completed");
    assert_eq!(zcard_completed(&url, &line).await, 0);

    flush_line(&url, &line).await;
}

/// Proves the *full chain* fires on Redis: leader election picks a winner,
/// `cleanup_loop` ticks on that node, and `FN_CLEANUP` drains the completed
/// ZSET — none of which we drive by hand here. Defends against regressions
/// where the loop is silently disabled on the Redis backend.
#[tokio::test]
async fn leader_cleanup_loop_drains_completed_zset() {
    let Some(url) = redis_url() else {
        eprintln!("skipping: REDIS_URL not set");
        return;
    };
    let line = fresh_line("retl");
    flush_line(&url, &line).await;

    let backend = RedisBackend::connect(RedisConfig {
        url: url.clone(),
        line: line.clone(),
    })
    .await
    .unwrap();

    let counter = Arc::new(AtomicUsize::new(0));
    // Aggressive cadence + zero retention so the loop has work to do
    // inside the test window.
    let queue: Queue<RedisBackend> = QueueBuilder::with_backend(backend)
        .register::<Count, _>(CountWorker {
            counter: counter.clone(),
        })
        .line(line.clone())
        .config(QueueConfig {
            fetch_poll_interval: Duration::from_millis(50),
            fetch_cooldown: Duration::from_millis(10),
            fetch_batch_size: 16,
            worker_concurrency: 4,
            heartbeat_interval: Duration::from_millis(100),
            sweep_interval: Duration::from_millis(500),
            stale_after: Duration::from_secs(10),
            scheduler_interval: Duration::from_millis(50),
            cleanup_interval: Duration::from_millis(200),
            completed_retention: Some(Duration::ZERO),
            failed_retention: None,
            cancelled_retention: None,
            batch_retention: None,
            leader_lease_secs: 5,
            ..QueueConfig::default()
        })
        .build();

    for n in 0..6 {
        queue.enqueue(&Count { n }).await.unwrap();
    }
    queue.start().unwrap();

    // First wait for the workers to finalize all 6 — they ZADD into the
    // completed index but the cleanup_loop hasn't ticked yet on its first
    // interval (the first `interval.tick()` consumes the immediate-fire).
    tokio::time::timeout(Duration::from_secs(5), async {
        while counter.load(Ordering::SeqCst) < 6 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("6 jobs should finalize within 5s");

    // Now wait for the leader cleanup_loop to drain the completed ZSET. We
    // never call backend.cleanup or .clean ourselves — only the loop does.
    tokio::time::timeout(Duration::from_secs(5), async {
        while zcard_completed(&url, &line).await > 0 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("leader cleanup_loop should drain completed ZSET within 5s");

    queue.shutdown().await.unwrap();
    flush_line(&url, &line).await;
}

/// `Backend::clean(grace, limit, Completed)` is the ad-hoc retention sweep
/// surface. Verifies the per-call `limit` cap by draining a 10-job backlog
/// in two calls of 5.
#[tokio::test]
async fn clean_caps_deletions_per_call() {
    use eddyq_core::backend::{Backend, CleanState};
    let Some(url) = redis_url() else {
        eprintln!("skipping: REDIS_URL not set");
        return;
    };
    let line = fresh_line("retx");
    flush_line(&url, &line).await;

    let counter = Arc::new(AtomicUsize::new(0));
    let queue = build_queue(&url, &line, counter.clone()).await;

    let jobs: Vec<Count> = (0..10).map(|n| Count { n }).collect();
    queue.enqueue_many(&jobs).await.unwrap();
    run_until(&queue, &counter, 10).await;

    assert_eq!(zcard_completed(&url, &line).await, 10);
    tokio::time::sleep(Duration::from_millis(50)).await;

    let backend = queue.backend().as_ref();
    let n1 = backend
        .clean(Duration::ZERO, 5, CleanState::Completed)
        .await
        .unwrap();
    assert_eq!(n1, 5, "first clean call should delete exactly 5");
    assert_eq!(zcard_completed(&url, &line).await, 5);

    let n2 = backend
        .clean(Duration::ZERO, 5, CleanState::Completed)
        .await
        .unwrap();
    assert_eq!(n2, 5, "second clean call drains the rest");
    assert_eq!(zcard_completed(&url, &line).await, 0);

    flush_line(&url, &line).await;
}

// ---- count-cap retention --------------------------------------------------
//
// The Redis backend mirrors the PG behavior: per-state count cap, OR-combined
// with the age window. Lua's `fn_cleanup` picks victims by negative-index
// ZRANGE (everything except the newest N) on top of the existing ZRANGEBYSCORE
// age sweep, deduping by ID. These tests pin both halves and their union.

/// Count cap alone: 6 finalized jobs, `completed_count: 2` → 4 deleted, the
/// 2 highest-scoring (newest finalize) remain. Age is unset, so age can't
/// account for the deletes.
#[tokio::test]
async fn cleanup_count_keeps_newest_n_completed() {
    use eddyq_core::backend::Backend;
    use eddyq_core::fetch::Retention;
    let Some(url) = redis_url() else {
        eprintln!("skipping: REDIS_URL not set");
        return;
    };
    let line = fresh_line("retcnt");
    flush_line(&url, &line).await;

    let counter = Arc::new(AtomicUsize::new(0));
    let queue = build_queue(&url, &line, counter.clone()).await;

    for n in 0..6 {
        queue.enqueue(&Count { n }).await.unwrap();
    }
    run_until(&queue, &counter, 6).await;
    assert_eq!(zcard_completed(&url, &line).await, 6);

    let backend = queue.backend().as_ref();
    let (c, _, _, _) = backend
        .cleanup(Retention {
            completed_count: Some(2),
            ..Retention::default()
        })
        .await
        .unwrap();
    assert_eq!(c, 4, "count cap should reap 4 of 6");
    assert_eq!(
        zcard_completed(&url, &line).await,
        2,
        "ZSET should hold exactly the 2 newest"
    );

    flush_line(&url, &line).await;
}

/// `completed_count: 0` is the "delete every finalized completed" shortcut —
/// the negative-index ZRANGE expands to the whole ZSET. Useful for tests but
/// also a legit prod knob (Redis users who don't want any completed retention).
#[tokio::test]
async fn cleanup_count_zero_drains_finalized_zset() {
    use eddyq_core::backend::Backend;
    use eddyq_core::fetch::Retention;
    let Some(url) = redis_url() else {
        eprintln!("skipping: REDIS_URL not set");
        return;
    };
    let line = fresh_line("retc0");
    flush_line(&url, &line).await;

    let counter = Arc::new(AtomicUsize::new(0));
    let queue = build_queue(&url, &line, counter.clone()).await;

    for n in 0..3 {
        queue.enqueue(&Count { n }).await.unwrap();
    }
    run_until(&queue, &counter, 3).await;
    assert_eq!(zcard_completed(&url, &line).await, 3);

    let backend = queue.backend().as_ref();
    let (c, _, _, _) = backend
        .cleanup(Retention {
            completed_count: Some(0),
            ..Retention::default()
        })
        .await
        .unwrap();
    assert_eq!(c, 3);
    assert_eq!(zcard_completed(&url, &line).await, 0);

    flush_line(&url, &line).await;
}

/// OR semantics: age=0 (sweep anything finalized before now_ms) and count=4
/// would each pick different victim sets. Dedupe in Lua means the count is
/// the *total* reaped, not age + count separately.
#[tokio::test]
async fn cleanup_count_and_age_dedupe_in_lua() {
    use eddyq_core::backend::Backend;
    use eddyq_core::fetch::Retention;
    let Some(url) = redis_url() else {
        eprintln!("skipping: REDIS_URL not set");
        return;
    };
    let line = fresh_line("retcoa");
    flush_line(&url, &line).await;

    let counter = Arc::new(AtomicUsize::new(0));
    let queue = build_queue(&url, &line, counter.clone()).await;

    for n in 0..5 {
        queue.enqueue(&Count { n }).await.unwrap();
    }
    run_until(&queue, &counter, 5).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let backend = queue.backend().as_ref();
    // age=0 alone would reap all 5; count=2 alone would reap 3 of 5; together
    // (union, dedup'd) we still expect 5 deleted with 0 left.
    let (c, _, _, _) = backend
        .cleanup(Retention {
            completed_secs: Some(0),
            completed_count: Some(2),
            ..Retention::default()
        })
        .await
        .unwrap();
    assert_eq!(c, 5, "age + count must not double-count overlapping IDs");
    assert_eq!(zcard_completed(&url, &line).await, 0);

    flush_line(&url, &line).await;
}

/// Count is per-state. Tightening `completed_count` must not touch the
/// failed/cancelled ZSETs. Enqueues a mix of successful and failing jobs to
/// populate both completed and failed ZSETs, then sweeps only completed.
#[tokio::test]
async fn cleanup_count_per_state_isolation_redis() {
    use eddyq_core::backend::Backend;
    use eddyq_core::fetch::Retention;
    let Some(url) = redis_url() else {
        eprintln!("skipping: REDIS_URL not set");
        return;
    };
    let line = fresh_line("retciso");
    flush_line(&url, &line).await;

    let counter = Arc::new(AtomicUsize::new(0));
    let queue = build_queue(&url, &line, counter.clone()).await;

    for n in 0..4 {
        queue.enqueue(&Count { n }).await.unwrap();
    }
    run_until(&queue, &counter, 4).await;
    assert_eq!(zcard_completed(&url, &line).await, 4);
    let failed_before = zcard_failed(&url, &line).await;

    let backend = queue.backend().as_ref();
    let (c, f, x, _) = backend
        .cleanup(Retention {
            completed_count: Some(1),
            ..Retention::default()
        })
        .await
        .unwrap();
    assert_eq!(c, 3);
    assert_eq!(f, 0, "failed ZSET must be untouched");
    assert_eq!(x, 0, "cancelled ZSET must be untouched");
    assert_eq!(zcard_completed(&url, &line).await, 1);
    assert_eq!(zcard_failed(&url, &line).await, failed_before);

    flush_line(&url, &line).await;
}

/// Stalled-recovery semantics on Redis. Manually plant a "stale" running
/// job (no live worker) by writing it directly to the active ZSET with an
/// old score, then call `sweep_stale` via the backend. First sweep: stalled
/// recovers free. Second sweep: budget exhausted, row moves to `failed`.
#[tokio::test]
async fn stalled_count_recovery_and_dlq() {
    use eddyq_core::EnqueueOptions;
    use eddyq_core::backend::Backend;
    let Some(url) = redis_url() else {
        eprintln!("skipping: REDIS_URL not set");
        return;
    };
    let line = fresh_line("stalled");
    flush_line(&url, &line).await;

    let counter = Arc::new(AtomicUsize::new(0));
    let queue = build_queue(&url, &line, counter.clone()).await;

    // Enqueue with maxAttempts=1, maxStalledCount=1 (one free crash, then fail).
    let opts = EnqueueOptions {
        max_attempts: Some(1),
        max_stalled_count: Some(1),
        ..Default::default()
    };
    let res = queue.enqueue_with(&Count { n: 42 }, opts).await.unwrap();
    let id = match res {
        eddyq_core::EnqueueResult::Inserted(id) => id,
        other => panic!("expected Inserted, got {:?}", other),
    };

    // Simulate a worker that claimed and then died: move the job to active,
    // mark it running with an ancient lock so sweep treats it as stale.
    let client = redis::Client::open(url.clone()).unwrap();
    let mut conn = client.get_multiplexed_async_connection().await.unwrap();
    let prefix = format!("{{{}}}", line);
    let job_key = format!("{}:job:{}", prefix, id);
    let wait_key = format!("{}:wait:default", prefix);
    let active_key = format!("{}:active", prefix);

    async fn stage_stale_running(
        conn: &mut redis::aio::MultiplexedConnection,
        wait_key: &str,
        active_key: &str,
        job_key: &str,
        id: i64,
    ) {
        let _: redis::Value = redis::cmd("ZREM")
            .arg(wait_key)
            .arg(id)
            .query_async(conn)
            .await
            .unwrap();
        let _: redis::Value = redis::cmd("ZADD")
            .arg(active_key)
            .arg(0_i64)
            .arg(id)
            .query_async(conn)
            .await
            .unwrap();
        let _: redis::Value = redis::cmd("HSET")
            .arg(job_key)
            .arg("state")
            .arg("running")
            .arg("locked_at")
            .arg("0")
            .arg("attempt")
            .arg("1")
            .query_async(conn)
            .await
            .unwrap();
    }

    stage_stale_running(&mut conn, &wait_key, &active_key, &job_key, id).await;

    // First sweep: stalled_count 0→1 ≤ max=1 → recover.
    let backend = queue.backend().as_ref();
    let n = backend.sweep_stale(Duration::from_millis(1)).await.unwrap();
    assert_eq!(n, 1);

    let fields: Vec<String> = redis::cmd("HMGET")
        .arg(&job_key)
        .arg("state")
        .arg("stalled_count")
        .arg("attempt")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(fields[0], "pending");
    assert_eq!(fields[1], "1");
    assert_eq!(
        fields[2], "0",
        "attempt decremented; handler budget preserved"
    );

    // Re-stage as stale running for the second sweep.
    stage_stale_running(&mut conn, &wait_key, &active_key, &job_key, id).await;

    // Second sweep: stalled_count 1→2 > max=1 → fail.
    let n = backend.sweep_stale(Duration::from_millis(1)).await.unwrap();
    assert_eq!(n, 1);

    let fields: Vec<String> = redis::cmd("HMGET")
        .arg(&job_key)
        .arg("state")
        .arg("stalled_count")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(fields[0], "failed");
    assert_eq!(fields[1], "2");

    flush_line(&url, &line).await;
}

/// `reclaim_in_flight` on Redis — new behavior added in the stalled-count
/// PR (it never had a DLQ branch before). Verify: under cap, the row goes
/// back to pending; over cap, it moves to failed. Mirrors the PG
/// `shutdown_force_reclaims_in_flight` test on the Redis backend.
#[tokio::test]
async fn reclaim_in_flight_recovers_then_dlqs() {
    use eddyq_core::EnqueueOptions;
    use eddyq_core::backend::Backend;
    let Some(url) = redis_url() else {
        eprintln!("skipping: REDIS_URL not set");
        return;
    };
    let line = fresh_line("reclaim");
    flush_line(&url, &line).await;

    let counter = Arc::new(AtomicUsize::new(0));
    let queue = build_queue(&url, &line, counter.clone()).await;

    let opts = EnqueueOptions {
        max_attempts: Some(1),
        max_stalled_count: Some(1),
        ..Default::default()
    };
    let res = queue.enqueue_with(&Count { n: 7 }, opts).await.unwrap();
    let id = match res {
        eddyq_core::EnqueueResult::Inserted(id) => id,
        other => panic!("expected Inserted, got {:?}", other),
    };

    let client = redis::Client::open(url.clone()).unwrap();
    let mut conn = client.get_multiplexed_async_connection().await.unwrap();
    let prefix = format!("{{{}}}", line);
    let job_key = format!("{}:job:{}", prefix, id);
    let wait_key = format!("{}:wait:default", prefix);
    let active_key = format!("{}:active", prefix);

    // Stage as in-flight on this pod, then reclaim.
    let _: redis::Value = redis::cmd("ZREM")
        .arg(&wait_key)
        .arg(id)
        .query_async(&mut conn)
        .await
        .unwrap();
    let _: redis::Value = redis::cmd("ZADD")
        .arg(&active_key)
        .arg(0_i64)
        .arg(id)
        .query_async(&mut conn)
        .await
        .unwrap();
    let _: redis::Value = redis::cmd("HSET")
        .arg(&job_key)
        .arg("state")
        .arg("running")
        .arg("attempt")
        .arg("1")
        .query_async(&mut conn)
        .await
        .unwrap();

    // First reclaim: stalled 0→1 ≤ max=1 → recover.
    let backend = queue.backend().as_ref();
    let n = backend.reclaim_in_flight(&[id]).await.unwrap();
    assert_eq!(n, 1);

    let fields: Vec<String> = redis::cmd("HMGET")
        .arg(&job_key)
        .arg("state")
        .arg("stalled_count")
        .arg("attempt")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(fields[0], "pending");
    assert_eq!(fields[1], "1");
    assert_eq!(fields[2], "0", "attempt decremented on recover");

    // Re-stage as in-flight and reclaim again — now stalled 1→2 > 1 → fail.
    let _: redis::Value = redis::cmd("ZREM")
        .arg(&wait_key)
        .arg(id)
        .query_async(&mut conn)
        .await
        .unwrap();
    let _: redis::Value = redis::cmd("ZADD")
        .arg(&active_key)
        .arg(0_i64)
        .arg(id)
        .query_async(&mut conn)
        .await
        .unwrap();
    let _: redis::Value = redis::cmd("HSET")
        .arg(&job_key)
        .arg("state")
        .arg("running")
        .arg("attempt")
        .arg("1")
        .query_async(&mut conn)
        .await
        .unwrap();

    let n = backend.reclaim_in_flight(&[id]).await.unwrap();
    assert_eq!(n, 1);

    let fields: Vec<String> = redis::cmd("HMGET")
        .arg(&job_key)
        .arg("state")
        .arg("stalled_count")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(fields[0], "failed");
    assert_eq!(fields[1], "2");

    flush_line(&url, &line).await;
}

/// Per-job `max_stalled_count` override on Redis. The field has to make
/// it from the Rust `EnqueueOptions` → `DynEnqueue` → Lua ARGV → job hash.
#[tokio::test]
async fn per_job_max_stalled_count_override_redis() {
    use eddyq_core::EnqueueOptions;
    let Some(url) = redis_url() else {
        eprintln!("skipping: REDIS_URL not set");
        return;
    };
    let line = fresh_line("override");
    flush_line(&url, &line).await;

    let counter = Arc::new(AtomicUsize::new(0));
    let queue = build_queue(&url, &line, counter.clone()).await;

    let opts = EnqueueOptions {
        max_stalled_count: Some(7),
        ..Default::default()
    };
    let res = queue.enqueue_with(&Count { n: 1 }, opts).await.unwrap();
    let id = match res {
        eddyq_core::EnqueueResult::Inserted(id) => id,
        other => panic!("expected Inserted, got {:?}", other),
    };

    let client = redis::Client::open(url.clone()).unwrap();
    let mut conn = client.get_multiplexed_async_connection().await.unwrap();
    let prefix = format!("{{{}}}", line);
    let job_key = format!("{}:job:{}", prefix, id);

    let stored: String = redis::cmd("HGET")
        .arg(&job_key)
        .arg("max_stalled_count")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(stored, "7");

    flush_line(&url, &line).await;
}

/// Pre-upgrade compatibility on Redis: a job hash written before the
/// stalled_count fields existed is read with `tonumber(field) or default`
/// in Lua, so sweep recovers it as if it had stalled_count=0,
/// max_stalled_count=1 from the start. Defends against the deploy window
/// where pre-bump pods write hashes and post-bump pods read them.
#[tokio::test]
async fn pre_upgrade_hash_recovers_cleanly_redis() {
    use eddyq_core::backend::Backend;
    let Some(url) = redis_url() else {
        eprintln!("skipping: REDIS_URL not set");
        return;
    };
    let line = fresh_line("preup");
    flush_line(&url, &line).await;

    let counter = Arc::new(AtomicUsize::new(0));
    let queue = build_queue(&url, &line, counter.clone()).await;

    // Hand-write a job hash WITHOUT stalled_count or max_stalled_count.
    // Simulates a hash that was enqueued by a pre-upgrade pod.
    let client = redis::Client::open(url.clone()).unwrap();
    let mut conn = client.get_multiplexed_async_connection().await.unwrap();
    let prefix = format!("{{{}}}", line);
    let id: i64 = redis::cmd("INCR")
        .arg(format!("{}:idgen", prefix))
        .query_async(&mut conn)
        .await
        .unwrap();
    let job_key = format!("{}:job:{}", prefix, id);
    let active_key = format!("{}:active", prefix);
    let _: redis::Value = redis::cmd("HSET")
        .arg(&job_key)
        .arg("id")
        .arg(id)
        .arg("kind")
        .arg("count")
        .arg("payload")
        .arg("{\"n\":1}")
        .arg("priority")
        .arg("0")
        .arg("max_attempts")
        .arg("3")
        .arg("attempt")
        .arg("1")
        .arg("state")
        .arg("running")
        .arg("queue")
        .arg("default")
        .arg("group_key")
        .arg("")
        .arg("unique_key")
        .arg("")
        .arg("tags")
        .arg("[]")
        .arg("metadata")
        .arg("{}")
        .arg("remove_on_complete")
        .arg("")
        .arg("remove_on_fail")
        .arg("")
        .arg("scheduled_at")
        .arg("0")
        .arg("created_at")
        .arg("0")
        .arg("locked_at")
        .arg("0")
        // Notably: NO `stalled_count`, NO `max_stalled_count`.
        .query_async(&mut conn)
        .await
        .unwrap();
    let _: redis::Value = redis::cmd("ZADD")
        .arg(&active_key)
        .arg(0_i64)
        .arg(id)
        .query_async(&mut conn)
        .await
        .unwrap();

    let backend = queue.backend().as_ref();
    let n = backend.sweep_stale(Duration::from_millis(1)).await.unwrap();
    assert_eq!(n, 1);

    let fields: Vec<String> = redis::cmd("HMGET")
        .arg(&job_key)
        .arg("state")
        .arg("stalled_count")
        .arg("attempt")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(fields[0], "pending");
    assert_eq!(
        fields[1], "1",
        "Lua's `tonumber(...) or 0` fallback applied; stalled_count now exists"
    );
    assert_eq!(fields[2], "0");

    flush_line(&url, &line).await;
}

// ====================================================================
// Per-job retention: prune teardown + queue scoping
// ====================================================================

async fn redis_conn(url: &str) -> redis::aio::MultiplexedConnection {
    redis::Client::open(url)
        .unwrap()
        .get_multiplexed_async_connection()
        .await
        .unwrap()
}

async fn zcard(conn: &mut redis::aio::MultiplexedConnection, key: &str) -> i64 {
    redis::cmd("ZCARD")
        .arg(key)
        .query_async(conn)
        .await
        .unwrap()
}

async fn scard(conn: &mut redis::aio::MultiplexedConnection, key: &str) -> i64 {
    redis::cmd("SCARD")
        .arg(key)
        .query_async(conn)
        .await
        .unwrap()
}

/// Like `build_queue`, but the workers only pull from `queues`, and no
/// queue-default cleanup is configured, so per-job rules are the only thing
/// that can delete a finalized job.
async fn build_queue_on(
    url: &str,
    line: &str,
    counter: Arc<AtomicUsize>,
    queues: &[&str],
) -> Queue<RedisBackend> {
    let backend = RedisBackend::connect(RedisConfig {
        url: url.to_owned(),
        line: line.to_owned(),
    })
    .await
    .expect("redis backend connect");

    QueueBuilder::with_backend(backend)
        .register::<Count, _>(CountWorker { counter })
        .config(QueueConfig {
            completed_retention: None,
            failed_retention: None,
            cancelled_retention: None,
            batch_retention: None,
            completed_retention_count: None,
            failed_retention_count: None,
            cancelled_retention_count: None,
            batch_retention_count: None,
            ..fast_config()
        })
        .subscribe_to(queues.iter().copied())
        .line(line.to_owned())
        .build()
}

/// Wait until nothing on `line` is running: every claimed job has been
/// finalized and its retention rule applied.
async fn wait_idle(conn: &mut redis::aio::MultiplexedConnection, line: &str) {
    let active = format!("{{{}}}:active", line);
    tokio::time::timeout(Duration::from_secs(5), async {
        while zcard(conn, &active).await > 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("running jobs should finalize within 5s");
}

/// A job enqueued by `enqueue_tracked`, with the keys it owns.
#[derive(Debug)]
struct Tracked {
    id: i64,
    queue: String,
    unique_key: String,
}

/// Enqueue `n` jobs on `queue_name`, each with a unique key, a tag, and a
/// seeded error-log entry, so every key a prune must tear down exists.
async fn enqueue_tracked(
    queue: &Queue<RedisBackend>,
    url: &str,
    line: &str,
    queue_name: &str,
    n: u64,
    rule: Option<eddyq_core::RetentionRule>,
) -> Vec<Tracked> {
    use eddyq_core::{EnqueueOptions, EnqueueResult};
    let mut conn = redis_conn(url).await;
    let mut jobs = Vec::new();
    for i in 0..n {
        let unique_key = format!("{}-{}", queue_name, uuid::Uuid::new_v4().simple());
        let opts = EnqueueOptions {
            queue: Some(queue_name.to_owned()),
            unique_key: Some(unique_key.clone()),
            tags: Some(vec![format!("tag-{}", queue_name)]),
            remove_on_complete: rule.clone(),
            ..Default::default()
        };
        let EnqueueResult::Inserted(id) = queue.enqueue_with(&Count { n: i }, opts).await.unwrap()
        else {
            panic!("expected Inserted");
        };
        let _: redis::Value = redis::cmd("RPUSH")
            .arg(format!("{{{}}}:job:{}:errors", line, id))
            .arg(r#"{"message":"seeded"}"#)
            .query_async(&mut conn)
            .await
            .unwrap();
        jobs.push(Tracked {
            id,
            queue: queue_name.to_owned(),
            unique_key,
        });
    }
    jobs
}

/// Every key and index entry a completed job owns, and whether it exists.
async fn completed_footprint(
    conn: &mut redis::aio::MultiplexedConnection,
    line: &str,
    job: &Tracked,
) -> Vec<(&'static str, bool)> {
    let p = format!("{{{}}}", line);
    let (id, q) = (job.id, &job.queue);
    #[allow(clippy::type_complexity)]
    let (hash, errors, unique, queue_set, kind_set, tag_set, global, per_queue): (
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        Option<f64>,
        Option<f64>,
    ) = redis::pipe()
        .cmd("EXISTS")
        .arg(format!("{}:job:{}", p, id))
        .cmd("EXISTS")
        .arg(format!("{}:job:{}:errors", p, id))
        .cmd("EXISTS")
        .arg(format!("{}:unique:{}", p, job.unique_key))
        .cmd("SISMEMBER")
        .arg(format!("{}:queue:{}", p, q))
        .arg(id)
        .cmd("SISMEMBER")
        .arg(format!("{}:kind:count", p))
        .arg(id)
        .cmd("SISMEMBER")
        .arg(format!("{}:tag:tag-{}", p, q))
        .arg(id)
        .cmd("ZSCORE")
        .arg(format!("{}:completed", p))
        .arg(id)
        .cmd("ZSCORE")
        .arg(format!("{}:nq:{}:completed", p, q))
        .arg(id)
        .query_async(conn)
        .await
        .unwrap();
    vec![
        ("job hash", hash == 1),
        ("error log", errors == 1),
        ("unique key", unique == 1),
        ("queue set", queue_set == 1),
        ("kind set", kind_set == 1),
        ("tag set", tag_set == 1),
        ("global completed zset", global.is_some()),
        ("per-queue completed zset", per_queue.is_some()),
    ]
}

async fn assert_kept(conn: &mut redis::aio::MultiplexedConnection, line: &str, job: &Tracked) {
    let fp = completed_footprint(conn, line, job).await;
    assert!(
        fp.iter().all(|(_, present)| *present),
        "job {} ({}) should be fully kept: {:?}",
        job.id,
        job.queue,
        fp
    );
}

async fn assert_pruned(conn: &mut redis::aio::MultiplexedConnection, line: &str, job: &Tracked) {
    let fp = completed_footprint(conn, line, job).await;
    assert!(
        fp.iter().all(|(_, present)| !*present),
        "job {} ({}) should be fully torn down: {:?}",
        job.id,
        job.queue,
        fp
    );
}

/// `removeOnComplete: { count: K }` prunes the surplus of the finalizing
/// job's own queue by deleting those jobs outright: HASH, error log, unique
/// key, and every index entry. Another queue's jobs on the same line are
/// never victims, even though they are older and share the global ZSET.
#[tokio::test]
async fn per_job_retention_count_prunes_own_queue_and_deletes_victims() {
    use eddyq_core::RetentionRule;
    let Some(url) = redis_url() else {
        eprintln!("skipping: REDIS_URL not set");
        return;
    };
    let line = fresh_line("retcq");
    flush_line(&url, &line).await;
    let p = format!("{{{}}}", line);

    // Queue qb finalizes first, without a rule, so its jobs are the oldest
    // entries in the global completed ZSET.
    let b_done = Arc::new(AtomicUsize::new(0));
    let qb = build_queue_on(&url, &line, b_done.clone(), &["qb"]).await;
    let b_jobs = enqueue_tracked(&qb, &url, &line, "qb", 4, None).await;
    run_until(&qb, &b_done, 4).await;

    let keep: u32 = 3;
    let a_done = Arc::new(AtomicUsize::new(0));
    let qa = build_queue_on(&url, &line, a_done.clone(), &["qa"]).await;
    let rule = Some(RetentionRule::keep_count(keep));
    let a_jobs = enqueue_tracked(&qa, &url, &line, "qa", 8, rule).await;
    run_until(&qa, &a_done, 8).await;

    let mut conn = redis_conn(&url).await;
    wait_idle(&mut conn, &line).await;

    let mut kept = 0;
    for job in &a_jobs {
        let fp = completed_footprint(&mut conn, &line, job).await;
        if fp.iter().all(|(_, present)| *present) {
            kept += 1;
        } else {
            assert!(
                fp.iter().all(|(_, present)| !*present),
                "pruned job {} left keys behind: {:?}",
                job.id,
                fp
            );
        }
    }
    assert_eq!(
        kept, keep,
        "count rule keeps exactly the newest {keep} of qa"
    );
    for job in &b_jobs {
        assert_kept(&mut conn, &line, job).await;
    }

    let keep = i64::from(keep);
    assert_eq!(
        zcard(&mut conn, &format!("{}:nq:qa:completed", p)).await,
        keep
    );
    assert_eq!(zcard(&mut conn, &format!("{}:nq:qb:completed", p)).await, 4);
    assert_eq!(
        zcard_completed(&url, &line).await,
        keep + 4,
        "global ZSET holds qa's kept jobs plus every qb job"
    );
    assert_eq!(scard(&mut conn, &format!("{}:queue:qa", p)).await, keep);
    assert_eq!(
        scard(&mut conn, &format!("{}:kind:count", p)).await,
        keep + 4
    );

    flush_line(&url, &line).await;
}

/// `removeOnComplete: { age }` deletes the finalizing queue's entries older
/// than the window, with full teardown. Another queue's entries past the
/// same cutoff are left alone.
#[tokio::test]
async fn per_job_retention_age_prunes_own_queue_and_deletes_victims() {
    use eddyq_core::RetentionRule;
    let Some(url) = redis_url() else {
        eprintln!("skipping: REDIS_URL not set");
        return;
    };
    let line = fresh_line("retaq");
    flush_line(&url, &line).await;
    let p = format!("{{{}}}", line);
    let rule = Some(RetentionRule::keep_age(Duration::from_secs(60)));

    let b_done = Arc::new(AtomicUsize::new(0));
    let qb = build_queue_on(&url, &line, b_done.clone(), &["qb"]).await;
    let b_jobs = enqueue_tracked(&qb, &url, &line, "qb", 3, None).await;
    run_until(&qb, &b_done, 3).await;

    let a_done = Arc::new(AtomicUsize::new(0));
    let qa = build_queue_on(&url, &line, a_done.clone(), &["qa"]).await;
    let old_a = enqueue_tracked(&qa, &url, &line, "qa", 5, rule.clone()).await;
    run_until(&qa, &a_done, 5).await;

    let mut conn = redis_conn(&url).await;
    wait_idle(&mut conn, &line).await;
    for job in &old_a {
        assert_kept(&mut conn, &line, job).await;
    }

    // Backdate every finalized entry of both queues past the 60s window.
    for job in b_jobs.iter().chain(&old_a) {
        let _: redis::Value = redis::pipe()
            .cmd("ZADD")
            .arg(format!("{}:completed", p))
            .arg("XX")
            .arg(0)
            .arg(job.id)
            .cmd("ZADD")
            .arg(format!("{}:nq:{}:completed", p, job.queue))
            .arg("XX")
            .arg(0)
            .arg(job.id)
            .query_async(&mut conn)
            .await
            .unwrap();
    }

    // One fresh qa finalize applies the rule.
    let a2_done = Arc::new(AtomicUsize::new(0));
    let qa2 = build_queue_on(&url, &line, a2_done.clone(), &["qa"]).await;
    let new_a = enqueue_tracked(&qa2, &url, &line, "qa", 1, rule).await;
    run_until(&qa2, &a2_done, 1).await;
    wait_idle(&mut conn, &line).await;

    for job in &old_a {
        assert_pruned(&mut conn, &line, job).await;
    }
    assert_kept(&mut conn, &line, &new_a[0]).await;
    for job in &b_jobs {
        assert_kept(&mut conn, &line, job).await;
    }
    assert_eq!(zcard(&mut conn, &format!("{}:nq:qa:completed", p)).await, 1);
    assert_eq!(zcard(&mut conn, &format!("{}:nq:qb:completed", p)).await, 3);
    assert_eq!(zcard_completed(&url, &line).await, 4);
    assert_eq!(scard(&mut conn, &format!("{}:queue:qa", p)).await, 1);
    assert_eq!(scard(&mut conn, &format!("{}:kind:count", p)).await, 4);

    flush_line(&url, &line).await;
}

/// One finalize deletes at most `RETENTION_PRUNE_CAP` (500) victims. The
/// rest stay indexed in both ZSETs, so the next finalize reaps them rather
/// than leaking them.
#[tokio::test]
async fn per_job_retention_prune_is_capped_per_finalize() {
    use eddyq_core::RetentionRule;
    let Some(url) = redis_url() else {
        eprintln!("skipping: REDIS_URL not set");
        return;
    };
    let line = fresh_line("retcap");
    flush_line(&url, &line).await;
    let p = format!("{{{}}}", line);
    const STAGED: i64 = 600;
    const CAP: i64 = 500;

    // Stage a backlog of long-finalized qa jobs, far outside the age window.
    let mut conn = redis_conn(&url).await;
    let staged: Vec<i64> = (0..STAGED).map(|i| 1_000_000 + i).collect();
    let mut pipe = redis::pipe();
    for id in &staged {
        pipe.cmd("HSET")
            .arg(format!("{}:job:{}", p, id))
            .arg("id")
            .arg(id)
            .arg("kind")
            .arg("count")
            .arg("queue")
            .arg("qa")
            .arg("state")
            .arg("completed")
            .arg("tags")
            .arg("[]")
            .arg("unique_key")
            .arg("")
            .ignore()
            .cmd("RPUSH")
            .arg(format!("{}:job:{}:errors", p, id))
            .arg(r#"{"message":"seeded"}"#)
            .ignore()
            .cmd("SADD")
            .arg(format!("{}:queue:qa", p))
            .arg(id)
            .ignore()
            .cmd("SADD")
            .arg(format!("{}:kind:count", p))
            .arg(id)
            .ignore()
            .cmd("ZADD")
            .arg(format!("{}:completed", p))
            .arg(0)
            .arg(id)
            .ignore()
            .cmd("ZADD")
            .arg(format!("{}:nq:qa:completed", p))
            .arg(0)
            .arg(id)
            .ignore();
    }
    let _: () = pipe.query_async(&mut conn).await.unwrap();

    /// Count of staged job HASHes plus error logs still present.
    async fn staged_keys_left(
        conn: &mut redis::aio::MultiplexedConnection,
        p: &str,
        staged: &[i64],
    ) -> i64 {
        let mut pipe = redis::pipe();
        for id in staged {
            pipe.cmd("EXISTS")
                .arg(format!("{}:job:{}", p, id))
                .arg(format!("{}:job:{}:errors", p, id));
        }
        let counts: Vec<i64> = pipe.query_async(conn).await.unwrap();
        counts.iter().sum()
    }

    let rule = Some(RetentionRule::keep_age(Duration::from_secs(60)));
    let first_done = Arc::new(AtomicUsize::new(0));
    let q1 = build_queue_on(&url, &line, first_done.clone(), &["qa"]).await;
    let first = enqueue_tracked(&q1, &url, &line, "qa", 1, rule.clone()).await;
    run_until(&q1, &first_done, 1).await;
    wait_idle(&mut conn, &line).await;

    assert_eq!(
        staged_keys_left(&mut conn, &p, &staged).await,
        (STAGED - CAP) * 2,
        "first finalize tears down exactly CAP staged jobs"
    );
    let remaining = STAGED - CAP + 1;
    assert_eq!(
        zcard(&mut conn, &format!("{}:nq:qa:completed", p)).await,
        remaining,
        "victims past the cap stay indexed for the next prune"
    );
    assert_eq!(zcard_completed(&url, &line).await, remaining);
    assert_eq!(
        scard(&mut conn, &format!("{}:queue:qa", p)).await,
        remaining
    );

    let second_done = Arc::new(AtomicUsize::new(0));
    let q2 = build_queue_on(&url, &line, second_done.clone(), &["qa"]).await;
    let second = enqueue_tracked(&q2, &url, &line, "qa", 1, rule).await;
    run_until(&q2, &second_done, 1).await;
    wait_idle(&mut conn, &line).await;

    assert_eq!(staged_keys_left(&mut conn, &p, &staged).await, 0);
    assert_eq!(zcard(&mut conn, &format!("{}:nq:qa:completed", p)).await, 2);
    assert_eq!(zcard_completed(&url, &line).await, 2);
    assert_eq!(scard(&mut conn, &format!("{}:queue:qa", p)).await, 2);
    assert_eq!(scard(&mut conn, &format!("{}:kind:count", p)).await, 2);
    assert_kept(&mut conn, &line, &first[0]).await;
    assert_kept(&mut conn, &line, &second[0]).await;

    flush_line(&url, &line).await;
}

/// `count: 0` keeps nothing: the queue's earlier jobs are pruned and the
/// finalizing job is dropped too, while other queues are untouched.
#[tokio::test]
async fn per_job_retention_count_zero_drops_self_and_prunes_own_queue() {
    use eddyq_core::RetentionRule;
    let Some(url) = redis_url() else {
        eprintln!("skipping: REDIS_URL not set");
        return;
    };
    let line = fresh_line("retc0");
    flush_line(&url, &line).await;
    let p = format!("{{{}}}", line);

    let b_done = Arc::new(AtomicUsize::new(0));
    let qb = build_queue_on(&url, &line, b_done.clone(), &["qb"]).await;
    let b_jobs = enqueue_tracked(&qb, &url, &line, "qb", 2, None).await;
    run_until(&qb, &b_done, 2).await;

    let a_done = Arc::new(AtomicUsize::new(0));
    let qa = build_queue_on(&url, &line, a_done.clone(), &["qa"]).await;
    let old_a = enqueue_tracked(&qa, &url, &line, "qa", 3, None).await;
    run_until(&qa, &a_done, 3).await;

    let a2_done = Arc::new(AtomicUsize::new(0));
    let qa2 = build_queue_on(&url, &line, a2_done.clone(), &["qa"]).await;
    let rule = Some(RetentionRule::keep_count(0));
    let dropper = enqueue_tracked(&qa2, &url, &line, "qa", 1, rule).await;
    run_until(&qa2, &a2_done, 1).await;

    let mut conn = redis_conn(&url).await;
    wait_idle(&mut conn, &line).await;
    for job in old_a.iter().chain(&dropper) {
        assert_pruned(&mut conn, &line, job).await;
    }
    for job in &b_jobs {
        assert_kept(&mut conn, &line, job).await;
    }
    assert_eq!(zcard(&mut conn, &format!("{}:nq:qa:completed", p)).await, 0);
    assert_eq!(zcard_completed(&url, &line).await, 2);
    assert_eq!(scard(&mut conn, &format!("{}:queue:qa", p)).await, 0);

    flush_line(&url, &line).await;
}

/// The Redis error log keeps only the newest `MAX_ERROR_ENTRIES` entries,
/// and each stored entry has its `message` / `stack` capped.
#[tokio::test]
async fn error_log_is_capped_and_truncated_redis() {
    use eddyq_core::backend::Backend;
    use eddyq_core::error::{
        HandlerFailure, MAX_ERROR_ENTRIES, MAX_ERROR_MESSAGE_BYTES, MAX_ERROR_STACK_BYTES,
    };
    let Some(url) = redis_url() else {
        eprintln!("skipping: REDIS_URL not set");
        return;
    };
    let line = fresh_line("errcap");
    flush_line(&url, &line).await;

    let counter = Arc::new(AtomicUsize::new(0));
    let queue = build_queue(&url, &line, counter).await;
    let eddyq_core::EnqueueResult::Inserted(id) = queue.enqueue(&Count { n: 1 }).await.unwrap()
    else {
        panic!("expected Inserted");
    };

    let mut conn = redis_conn(&url).await;
    let job_key = format!("{{{}}}:job:{}", line, id);
    let err_key = format!("{}:errors", job_key);
    let worker_id = uuid::Uuid::new_v4();
    let backend = queue.backend().as_ref();

    let appends = MAX_ERROR_ENTRIES + 5;
    for attempt in 1..=appends {
        // Hand the job to `worker_id` so `eddyq_fail` accepts the verdict.
        let _: redis::Value = redis::cmd("HSET")
            .arg(&job_key)
            .arg("state")
            .arg("running")
            .arg("locked_by")
            .arg(worker_id.to_string())
            .query_async(&mut conn)
            .await
            .unwrap();
        let failure = HandlerFailure {
            message: format!("attempt {} {}", attempt, "x".repeat(10_000)),
            name: Some("BigError".into()),
            stack: Some("s".repeat(20_000)),
            directive: None,
        };
        let retry_at = chrono::Utc::now() + chrono::Duration::hours(1);
        backend
            .mark_failed(id, worker_id, failure.as_error_entry(), Some(retry_at))
            .await
            .unwrap();
    }

    let raw: Vec<String> = redis::cmd("LRANGE")
        .arg(&err_key)
        .arg(0)
        .arg(-1)
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(
        raw.len(),
        MAX_ERROR_ENTRIES,
        "LTRIM keeps the newest entries"
    );
    for (i, entry) in raw.iter().enumerate() {
        let entry: serde_json::Value = serde_json::from_str(entry).unwrap();
        let message = entry["message"].as_str().unwrap();
        let stack = entry["stack"].as_str().unwrap();
        let attempt = appends - MAX_ERROR_ENTRIES + 1 + i;
        assert!(
            message.starts_with(&format!("attempt {} x", attempt)),
            "entry {i} should be attempt {attempt}, got {:?}",
            &message[..20]
        );
        assert!(message.len() <= MAX_ERROR_MESSAGE_BYTES);
        assert!(message.contains("...[truncated "));
        assert!(stack.len() <= MAX_ERROR_STACK_BYTES);
        assert!(stack.contains("...[truncated "));
    }

    flush_line(&url, &line).await;
}
