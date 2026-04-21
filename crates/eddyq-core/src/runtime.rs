use std::{sync::Arc, time::Duration};

use rand::Rng;
use sqlx::{PgPool, postgres::PgListener};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::{
    QueueConfig,
    fetch::{
        ClaimedJob, claim_batch, mark_completed, mark_failed, sweep_stale, update_heartbeat,
    },
    job::{JobContext, JobId},
    worker::WorkerRegistry,
};

pub(crate) const NOTIFY_CHANNEL: &str = "eddyq_job";

pub(crate) struct RuntimeHandles {
    pub fetcher: JoinHandle<()>,
    pub workers: Vec<JoinHandle<()>>,
    pub sweeper: JoinHandle<()>,
    pub scheduler: JoinHandle<()>,
    pub cleanup: JoinHandle<()>,
    pub listener: Option<JoinHandle<()>>,
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
            ))
        })
        .collect();

    let sweeper = tokio::spawn(sweeper_loop(
        pool.clone(),
        config.clone(),
        shutdown.clone(),
    ));

    let scheduler = tokio::spawn(scheduler_loop(
        pool.clone(),
        config.clone(),
        shutdown.clone(),
    ));

    let cleanup = tokio::spawn(cleanup_loop(
        pool.clone(),
        config.clone(),
        shutdown.clone(),
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
        let claimed =
            match claim_batch(&pool, worker_id, batch_size, &kinds, &queues).await {
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
                if let Err(db_err) =
                    mark_failed(&pool, job.id, job.worker_id, &err.to_string(), None).await
                {
                    error!(?db_err, "failed to mark unknown-kind job as failed");
                }
                continue;
            }
        };

        // Heartbeat ticker so the sweeper knows this worker is alive.
        let hb_pool = pool.clone();
        let hb_job_id = job.id;
        let hb_worker_id = job.worker_id;
        let hb_interval = config.heartbeat_interval;
        let hb_stop = CancellationToken::new();
        let hb_stop_child = hb_stop.clone();
        let heartbeat_task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(hb_interval);
            interval.tick().await; // immediate tick is free (just resets the schedule)
            loop {
                tokio::select! {
                    biased;
                    () = hb_stop_child.cancelled() => break,
                    _ = interval.tick() => {
                        if let Err(err) = update_heartbeat(&hb_pool, hb_job_id, hb_worker_id).await {
                            warn!(id = hb_job_id, ?err, "heartbeat update failed");
                        }
                    }
                }
            }
        });

        let fut = std::panic::AssertUnwindSafe(handler(job.payload.clone(), ctx.clone()));
        let result = futures_util::FutureExt::catch_unwind(fut).await;

        hb_stop.cancel();
        let _ = heartbeat_task.await;

        match result {
            Ok(Ok(())) => {
                if let Err(err) = mark_completed(&pool, job.id, job.worker_id).await {
                    error!(id = job.id, ?err, "failed to mark job completed");
                }
            }
            Ok(Err(err)) => {
                let retry_at = retry_schedule(job.attempt, job.max_attempts, &config);
                warn!(id = job.id, attempt = job.attempt, max = job.max_attempts, retry_at = ?retry_at, error = %err, "job failed");
                if let Err(db_err) =
                    mark_failed(&pool, job.id, job.worker_id, &err.to_string(), retry_at).await
                {
                    error!(id = job.id, ?db_err, "failed to record job failure");
                }
            }
            Err(panic) => {
                let msg = panic_message(&panic);
                let retry_at = retry_schedule(job.attempt, job.max_attempts, &config);
                error!(id = job.id, attempt = job.attempt, retry_at = ?retry_at, msg = %msg, "job panicked");
                if let Err(db_err) =
                    mark_failed(&pool, job.id, job.worker_id, &msg, retry_at).await
                {
                    error!(id = job.id, ?db_err, "failed to record panic");
                }
            }
        }
    }

    info!(worker = n, "eddyq worker stopped");
}

async fn sweeper_loop(pool: PgPool, config: QueueConfig, shutdown: CancellationToken) {
    info!("eddyq sweeper started");
    let mut interval = tokio::time::interval(config.sweep_interval);
    interval.tick().await; // immediate
    loop {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => break,
            _ = interval.tick() => {
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

async fn scheduler_loop(pool: PgPool, config: QueueConfig, shutdown: CancellationToken) {
    info!("eddyq scheduler started");
    let mut interval = tokio::time::interval(config.scheduler_interval);
    interval.tick().await;
    loop {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => break,
            _ = interval.tick() => {
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

async fn cleanup_loop(pool: PgPool, config: QueueConfig, shutdown: CancellationToken) {
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
            warn!(?err, "LISTEN connection failed, falling back to polling only");
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
}

// JobId import so the `id = hb_job_id` tracing field resolves cleanly.
#[allow(dead_code)]
fn _assert_job_id_type(_: JobId) {}
