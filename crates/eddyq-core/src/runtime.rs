use std::{
    collections::HashSet,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use rand::Rng;
use sqlx::{PgPool, postgres::PgListener};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::{
    QueueConfig,
    fetch::{ClaimedJob, claim_batch, mark_completed, mark_failed, sweep_stale, update_heartbeat_batch},
    job::{JobContext, JobId},
    leader::{self, LEADER_RESIGN_CHANNEL, MAINTENANCE_ROLE},
    worker::WorkerRegistry,
};

pub(crate) const NOTIFY_CHANNEL: &str = "eddyq_job";

/// Shared set of in-flight job IDs for the batch heartbeat task.
type InFlightJobs = Arc<std::sync::Mutex<HashSet<i64>>>;

pub(crate) struct RuntimeHandles {
    pub fetcher: JoinHandle<()>,
    pub workers: Vec<JoinHandle<()>>,
    pub sweeper: JoinHandle<()>,
    pub scheduler: JoinHandle<()>,
    pub cleanup: JoinHandle<()>,
    pub listener: Option<JoinHandle<()>>,
    pub heartbeat: JoinHandle<()>,
    pub leader: JoinHandle<()>,
}

pub(crate) fn start(
    pool: PgPool,
    registry: Arc<WorkerRegistry>,
    config: QueueConfig,
    queues: Vec<String>,
    shutdown: CancellationToken,
) -> RuntimeHandles {
    let (tx, rx) = mpsc::channel::<ClaimedJob>(config.worker_concurrency.max(1));
    let rx = Arc::new(tokio::sync::Mutex::new(rx));
    let wakeup = Arc::new(tokio::sync::Notify::new());

    // Shared in-flight job set for batch heartbeats.
    let in_flight: InFlightJobs = Arc::new(std::sync::Mutex::new(HashSet::new()));

    // Leader election state.
    let is_leader = Arc::new(AtomicBool::new(false));
    let maintenance_id = Uuid::new_v4();

    let fetcher = tokio::spawn(fetch_loop(
        pool.clone(),
        tx,
        config.clone(),
        registry.clone(),
        queues,
        shutdown.clone(),
        wakeup.clone(),
    ));

    let workers = (0..config.worker_concurrency)
        .map(|n| {
            tokio::spawn(worker_loop(
                n,
                pool.clone(),
                registry.clone(),
                rx.clone(),
                config.clone(),
                shutdown.clone(),
                in_flight.clone(),
            ))
        })
        .collect();

    let heartbeat = tokio::spawn(heartbeat_loop(
        pool.clone(),
        in_flight,
        config.clone(),
        shutdown.clone(),
    ));

    let leader = tokio::spawn(leader_loop(
        pool.clone(),
        maintenance_id,
        is_leader.clone(),
        config.clone(),
        shutdown.clone(),
    ));

    let sweeper = tokio::spawn(sweeper_loop(
        pool.clone(),
        config.clone(),
        shutdown.clone(),
        is_leader.clone(),
    ));

    let scheduler = tokio::spawn(scheduler_loop(
        pool.clone(),
        config.clone(),
        shutdown.clone(),
        is_leader.clone(),
    ));

    let cleanup = tokio::spawn(cleanup_loop(
        pool.clone(),
        config.clone(),
        shutdown.clone(),
        is_leader.clone(),
    ));

    let listener = if config.poll_only {
        None
    } else {
        Some(tokio::spawn(listener_loop(
            pool.clone(),
            wakeup.clone(),
            shutdown.clone(),
        )))
    };

    RuntimeHandles {
        fetcher,
        workers,
        sweeper,
        scheduler,
        cleanup,
        listener,
        heartbeat,
        leader,
    }
}

async fn fetch_loop(
    pool: PgPool,
    tx: mpsc::Sender<ClaimedJob>,
    config: QueueConfig,
    registry: Arc<WorkerRegistry>,
    queues: Vec<String>,
    shutdown: CancellationToken,
    wakeup: Arc<tokio::sync::Notify>,
) {
    let worker_id = Uuid::new_v4();
    let kinds = registry.kinds();
    info!(%worker_id, ?kinds, ?queues, "eddyq fetcher started");
    let _ = registry; // keep registry alive only for kinds snapshot

    loop {
        if shutdown.is_cancelled() {
            break;
        }

        let capacity = tx.capacity();
        if capacity == 0 {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => break,
                () = tokio::time::sleep(config.fetch_cooldown) => continue,
            }
        }

        let batch_size = config.fetch_batch_size.min(capacity);
        let kinds = registry.kinds();
        let claimed = match claim_batch(&pool, worker_id, batch_size, &kinds, &queues).await {
            Ok(rows) => rows,
            Err(err) => {
                error!(?err, "fetch failed");
                tokio::select! {
                    biased;
                    () = shutdown.cancelled() => break,
                    () = tokio::time::sleep(config.fetch_cooldown) => continue,
                }
            }
        };

        let got = claimed.len();
        for job in claimed {
            if tx.send(job).await.is_err() {
                debug!("worker channel closed, fetcher exiting");
                return;
            }
        }

        let sleep = if got == 0 {
            jitter(config.fetch_poll_interval)
        } else {
            config.fetch_cooldown
        };

        tokio::select! {
            biased;
            () = shutdown.cancelled() => break,
            () = wakeup.notified() => {
                // Immediate re-poll after a notify, but respect fetch_cooldown so a
                // notify storm can't trigger back-to-back fetches.
                tokio::select! {
                    () = shutdown.cancelled() => break,
                    () = tokio::time::sleep(config.fetch_cooldown) => {}
                }
            }
            () = tokio::time::sleep(sleep) => {}
        }
    }

    info!("eddyq fetcher stopped");
}

async fn worker_loop(
    n: usize,
    pool: PgPool,
    registry: Arc<WorkerRegistry>,
    rx: Arc<tokio::sync::Mutex<mpsc::Receiver<ClaimedJob>>>,
    config: QueueConfig,
    shutdown: CancellationToken,
    in_flight: InFlightJobs,
) {
    let worker_id = Uuid::new_v4();
    info!(worker = n, %worker_id, "eddyq worker started");

    loop {
        let job = {
            let mut guard = rx.lock().await;
            tokio::select! {
                biased;
                () = shutdown.cancelled() => break,
                next = guard.recv() => next,
            }
        };

        let Some(job) = job else { break };

        // Register this job as in-flight for the shared heartbeat loop.
        {
            let mut set = in_flight.lock().expect("in_flight lock poisoned");
            set.insert(job.id);
        }

        let ctx = JobContext {
            id: job.id,
            kind: job.kind.clone(),
            attempt: job.attempt,
            max_attempts: job.max_attempts,
            worker_id,
        };

        let handler = match registry.get(&job.kind) {
            Ok(h) => h,
            Err(err) => {
                warn!(id = job.id, kind = %job.kind, ?err, "no handler for job kind");
                let entry = crate::error::HandlerFailure {
                    message: err.to_string(),
                    name: Some("UnknownKind".into()),
                    ..Default::default()
                }
                .as_error_entry();
                {
                    let mut set = in_flight.lock().expect("in_flight lock poisoned");
                    set.remove(&job.id);
                }
                if let Err(db_err) = mark_failed(&pool, job.id, job.worker_id, entry, None).await {
                    error!(?db_err, "failed to mark unknown-kind job as failed");
                }
                continue;
            }
        };

        let inner = handler(job.payload.clone(), ctx.clone());
        let caught_fut = futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(inner));

        // If the queue has a configured default timeout, wrap the handler in
        // `tokio::time::timeout`. On timeout, synthesize a JobResult::Err so
        // the downstream retry/fail plumbing treats it like any other failure.
        let result = match job.timeout {
            Some(t) => match tokio::time::timeout(t, caught_fut).await {
                Ok(caught) => caught,
                Err(_elapsed) => Ok(Err(anyhow::anyhow!(format!("job timed out after {:?}", t)))),
            },
            None => caught_fut.await,
        };

        // Remove from in-flight set on all exit paths.
        {
            let mut set = in_flight.lock().expect("in_flight lock poisoned");
            set.remove(&job.id);
        }

        match result {
            Ok(Ok(value)) => {
                let stored = match value {
                    serde_json::Value::Null => None,
                    other => Some(other),
                };
                if let Err(err) = mark_completed(&pool, job.id, job.worker_id, stored).await {
                    error!(id = job.id, ?err, "failed to mark job completed");
                }
            }
            Ok(Err(err)) => {
                // If the handler attached a HandlerFailure (via downcast), honor
                // its directive. Otherwise build a minimal failure from the
                // anyhow display.
                let failure = err
                    .downcast_ref::<crate::error::HandlerFailure>()
                    .cloned()
                    .unwrap_or_else(|| crate::error::HandlerFailure::from_message(err.to_string()));
                let retry_at = match failure.directive {
                    Some(crate::error::Directive::Cancel) => None,
                    Some(crate::error::Directive::Retry { delay_ms }) => {
                        Some(chrono::Utc::now() + chrono::Duration::milliseconds(delay_ms as i64))
                    }
                    None => retry_schedule(job.attempt, job.max_attempts, &config),
                };
                warn!(
                    id = job.id, attempt = job.attempt, max = job.max_attempts,
                    retry_at = ?retry_at, directive = ?failure.directive,
                    error = %failure, "job failed"
                );
                if let Err(db_err) = mark_failed(
                    &pool,
                    job.id,
                    job.worker_id,
                    failure.as_error_entry(),
                    retry_at,
                )
                .await
                {
                    error!(id = job.id, ?db_err, "failed to record job failure");
                }
            }
            Err(panic) => {
                let msg = panic_message(&panic);
                let retry_at = retry_schedule(job.attempt, job.max_attempts, &config);
                error!(id = job.id, attempt = job.attempt, retry_at = ?retry_at, msg = %msg, "job panicked");
                let entry = crate::error::HandlerFailure {
                    message: msg,
                    name: Some("Panic".into()),
                    ..Default::default()
                }
                .as_error_entry();
                if let Err(db_err) =
                    mark_failed(&pool, job.id, job.worker_id, entry, retry_at).await
                {
                    error!(id = job.id, ?db_err, "failed to record panic");
                }
            }
        }
    }

    info!(worker = n, "eddyq worker stopped");
}

/// Single shared heartbeat loop — one pool acquire every `heartbeat_interval`
/// regardless of concurrency. Replaces per-job heartbeat tasks.
async fn heartbeat_loop(
    pool: PgPool,
    in_flight: InFlightJobs,
    config: QueueConfig,
    shutdown: CancellationToken,
) {
    info!("eddyq heartbeat loop started");
    let mut interval = tokio::time::interval(config.heartbeat_interval);
    interval.tick().await; // first tick fires immediately; skip it
    loop {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => break,
            _ = interval.tick() => {
                let ids: Vec<i64> = {
                    let set = in_flight.lock().expect("in_flight lock poisoned");
                    set.iter().copied().collect()
                };
                if ids.is_empty() {
                    continue;
                }
                if let Err(err) = update_heartbeat_batch(&pool, &ids).await {
                    warn!(?err, count = ids.len(), "batch heartbeat update failed");
                }
            }
        }
    }
    info!("eddyq heartbeat loop stopped");
}

/// Leader election loop — runs try_elect on a regular cadence. Sets `is_leader`
/// so other loops can gate maintenance work on leadership.
async fn leader_loop(
    pool: PgPool,
    worker_id: Uuid,
    is_leader: Arc<AtomicBool>,
    config: QueueConfig,
    shutdown: CancellationToken,
) {
    info!(%worker_id, "eddyq leader loop started");
    let refresh_interval = Duration::from_secs(config.leader_lease_secs / 3)
        .max(Duration::from_secs(5));
    let mut interval = tokio::time::interval(refresh_interval);

    // Subscribe to peer-resignation NOTIFYs so we can fire an immediate election
    // when the current leader gracefully shuts down. Optional — falls back to
    // tick-driven elections if the LISTEN connection can't be established.
    let mut resign_listener = match PgListener::connect_with(&pool).await {
        Ok(mut l) => match l.listen(LEADER_RESIGN_CHANNEL).await {
            Ok(()) => Some(l),
            Err(err) => {
                warn!(?err, "leader-resign LISTEN setup failed; relying on tick-driven elections");
                None
            }
        },
        Err(err) => {
            warn!(?err, "leader-resign LISTEN connection failed; relying on tick-driven elections");
            None
        }
    };

    let try_elect_once = |reason: &'static str, was: &Arc<AtomicBool>| {
        let pool = pool.clone();
        let was = was.clone();
        async move {
            match leader::try_elect(&pool, worker_id, MAINTENANCE_ROLE, config.leader_lease_secs).await {
                Ok(won) => {
                    let prev = was.swap(won, Ordering::Relaxed);
                    if won && !prev {
                        info!(%worker_id, reason, "elected as maintenance leader");
                    } else if !won && prev {
                        warn!(%worker_id, reason, "lost maintenance leadership");
                    }
                }
                Err(err) => {
                    warn!(?err, reason, "leader election failed");
                }
            }
        }
    };

    // Run an immediate election before entering the tick loop so maintenance
    // tasks (sweeper, scheduler, cleanup) can start working right away.
    try_elect_once("initial", &is_leader).await;
    interval.tick().await; // consume the first tick (already ran above)

    loop {
        // Build the recv future inline; if no listener we await a never-ready
        // future so the tokio::select! still has a valid arm.
        let recv: std::pin::Pin<Box<dyn std::future::Future<Output = sqlx::Result<sqlx::postgres::PgNotification>> + Send>> =
            match resign_listener.as_mut() {
                Some(l) => Box::pin(l.recv()),
                None => Box::pin(std::future::pending()),
            };

        tokio::select! {
            biased;
            () = shutdown.cancelled() => break,
            ev = recv => {
                match ev {
                    Ok(_) => {
                        // A peer resigned — if we're not the leader, race for the lease.
                        if !is_leader.load(Ordering::Relaxed) {
                            try_elect_once("peer_resigned", &is_leader).await;
                        }
                    }
                    Err(err) => {
                        warn!(?err, "leader-resign listener error");
                        // Drop the listener; subsequent loop iterations fall back to ticks.
                        resign_listener = None;
                    }
                }
            }
            _ = interval.tick() => {
                try_elect_once("tick", &is_leader).await;
            }
        }
    }

    // On shutdown, resign if we were leader so another pod can take over immediately.
    if is_leader.load(Ordering::Relaxed) {
        if let Err(err) = leader::resign(&pool, worker_id, MAINTENANCE_ROLE).await {
            warn!(?err, "leader resign failed on shutdown");
        } else {
            info!(%worker_id, "resigned maintenance leadership on shutdown");
        }
    }

    info!("eddyq leader loop stopped");
}

async fn sweeper_loop(
    pool: PgPool,
    config: QueueConfig,
    shutdown: CancellationToken,
    is_leader: Arc<AtomicBool>,
) {
    info!("eddyq sweeper started");
    let mut interval = tokio::time::interval(config.sweep_interval);
    interval.tick().await; // immediate
    loop {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => break,
            _ = interval.tick() => {
                if !is_leader.load(Ordering::Relaxed) {
                    continue;
                }
                match sweep_stale(&pool, config.stale_after).await {
                    Ok(0) => {}
                    Ok(n) => info!(recovered = n, "sweeper recovered stale jobs"),
                    Err(err) => error!(?err, "sweeper failed"),
                }
            }
        }
    }
    info!("eddyq sweeper stopped");
}

async fn scheduler_loop(
    pool: PgPool,
    config: QueueConfig,
    shutdown: CancellationToken,
    is_leader: Arc<AtomicBool>,
) {
    info!("eddyq scheduler started");
    let mut interval = tokio::time::interval(config.scheduler_interval);
    interval.tick().await;
    loop {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => break,
            _ = interval.tick() => {
                if !is_leader.load(Ordering::Relaxed) {
                    continue;
                }
                match crate::schedule::tick(&pool).await {
                    Ok(0) => {}
                    Ok(n) => info!(enqueued = n, "scheduler enqueued due jobs"),
                    Err(err) => warn!(?err, "scheduler tick failed"),
                }
            }
        }
    }
    info!("eddyq scheduler stopped");
}

async fn cleanup_loop(
    pool: PgPool,
    config: QueueConfig,
    shutdown: CancellationToken,
    is_leader: Arc<AtomicBool>,
) {
    // Short-circuit if no retention is configured — nothing to do.
    if config.completed_retention.is_none()
        && config.failed_retention.is_none()
        && config.cancelled_retention.is_none()
    {
        debug!("cleanup disabled (all retentions None)");
        return;
    }
    info!("eddyq cleanup started");
    let retention = crate::fetch::Retention {
        completed_secs: config.completed_retention.map(|d| d.as_secs()),
        failed_secs: config.failed_retention.map(|d| d.as_secs()),
        cancelled_secs: config.cancelled_retention.map(|d| d.as_secs()),
    };
    let mut interval = tokio::time::interval(config.cleanup_interval);
    interval.tick().await;
    loop {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => break,
            _ = interval.tick() => {
                if !is_leader.load(Ordering::Relaxed) {
                    continue;
                }
                match crate::fetch::cleanup(&pool, retention).await {
                    Ok((0, 0, 0)) => {}
                    Ok((c, f, x)) => info!(
                        completed = c, failed = f, cancelled = x,
                        "cleanup deleted old finalized jobs"
                    ),
                    Err(err) => warn!(?err, "cleanup failed"),
                }
            }
        }
    }
    info!("eddyq cleanup stopped");
}

async fn listener_loop(
    pool: PgPool,
    wakeup: Arc<tokio::sync::Notify>,
    shutdown: CancellationToken,
) {
    // Dedicated connection — PgListener owns it outside the pool.
    let mut listener = match PgListener::connect_with(&pool).await {
        Ok(l) => l,
        Err(err) => {
            warn!(
                ?err,
                "LISTEN connection failed, falling back to polling only"
            );
            return;
        }
    };

    if let Err(err) = listener.listen(NOTIFY_CHANNEL).await {
        warn!(?err, "LISTEN setup failed, falling back to polling only");
        return;
    }

    info!(channel = NOTIFY_CHANNEL, "eddyq listener started");

    loop {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => break,
            ev = listener.recv() => {
                match ev {
                    Ok(_) => wakeup.notify_one(),
                    Err(err) => {
                        warn!(?err, "listener error, will attempt to continue");
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                }
            }
        }
    }

    info!("eddyq listener stopped");
}

fn jitter(base: Duration) -> Duration {
    let nanos = u64::try_from(base.as_nanos().min(u128::from(u64::MAX))).unwrap_or(u64::MAX);
    let tenth = nanos / 10;
    let extra = if tenth == 0 {
        0
    } else {
        rand::thread_rng().gen_range(0..tenth)
    };
    base + Duration::from_nanos(extra.max(10_000_000))
}

fn panic_message(panic: &Box<dyn std::any::Any + Send + 'static>) -> String {
    if let Some(s) = panic.downcast_ref::<&'static str>() {
        (*s).to_owned()
    } else if let Some(s) = panic.downcast_ref::<String>() {
        s.clone()
    } else {
        "job panicked".to_owned()
    }
}

fn retry_schedule(
    attempt: i32,
    max_attempts: i32,
    config: &QueueConfig,
) -> Option<chrono::DateTime<chrono::Utc>> {
    if attempt >= max_attempts {
        None
    } else {
        Some(crate::retry::backoff_until(
            attempt,
            config.retry_base,
            config.retry_max,
        ))
    }
}

pub(crate) async fn await_all(handles: RuntimeHandles) {
    let _ = handles.fetcher.await;
    for h in handles.workers {
        let _ = h.await;
    }
    let _ = handles.sweeper.await;
    let _ = handles.scheduler.await;
    let _ = handles.cleanup.await;
    if let Some(h) = handles.listener {
        let _ = h.await;
    }
    let _ = handles.heartbeat.await;
    let _ = handles.leader.await;
}

// JobId import so the `id = job_id` tracing field resolves cleanly.
#[allow(dead_code)]
fn _assert_job_id_type(_: JobId) {}
