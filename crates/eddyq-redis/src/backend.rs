//! `RedisBackend` — Redis Functions–powered implementation of the eddyq
//! `Backend` trait. PR2 covers the hot path (enqueue, claim, ack/nack,
//! sweep, leader, delayed promotion, reclaim) plus per-job retention.
//! Differentiator methods (groups, schedules, rate limits, list_jobs) are
//! still stubbed to `Error::Unsupported` and land in PR3.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eddyq_core::backend::{Backend, BackendCaps, CleanState};
use eddyq_core::{
    BulkEnqueueResult, DynEnqueue, EnqueueResult, JobId, RetentionRule,
    fetch::{ClaimedJob, Retention},
    group::{Group, GroupRule, StoredRule},
    named_queue::NamedQueue,
    schedule::{Schedule, ScheduleDeclaration, SyncReport},
    stats::{JobList, JobStats, ListJobsFilter, Pagination},
};
use redis::aio::ConnectionManager;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::bootstrap::{ensure_loaded, with_retry};
use crate::functions::*;
use crate::keys;

/// Connection + namespace configuration for a Redis backend.
#[derive(Debug, Clone)]
pub struct RedisConfig {
    /// `redis://` URL. Sentinel and Cluster URLs land in PR3.
    pub url: String,
    /// Hash-tag namespace ("line"). All keys for a queue use `{<line>}` so
    /// they map to a single Redis Cluster slot. Default `"main"`.
    pub line: String,
}

impl Default for RedisConfig {
    fn default() -> Self {
        Self {
            url: "redis://127.0.0.1:6379".into(),
            line: "main".into(),
        }
    }
}

/// Redis Functions–backed `Backend`.
#[derive(Clone)]
pub struct RedisBackend {
    conn: ConnectionManager,
    line: String,
    /// Pre-formatted `{<line>}` prefix used as `KEYS[1]` for every FCALL.
    prefix: String,
}

impl std::fmt::Debug for RedisBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisBackend")
            .field("line", &self.line)
            .finish_non_exhaustive()
    }
}

impl RedisBackend {
    /// Connect, then bootstrap-load the `eddyq_v1` Redis Functions library.
    /// Idempotent — safe to call from multiple peers concurrently.
    pub async fn connect(config: RedisConfig) -> Result<Self, crate::Error> {
        let client = redis::Client::open(config.url.as_str())?;
        let mut conn = ConnectionManager::new(client).await?;
        ensure_loaded(&mut conn).await?;
        let prefix = keys::prefix(&config.line);
        Ok(Self {
            conn,
            line: config.line,
            prefix,
        })
    }

    /// Hash-tag namespace this backend uses for all its keys.
    pub fn line(&self) -> &str {
        &self.line
    }

    fn now_ms(&self) -> i64 {
        chrono::Utc::now().timestamp_millis()
    }

    fn conn_clone(&self) -> ConnectionManager {
        self.conn.clone()
    }

    /// Upsert a schedule. `interval_ms` of 0 means "cron-driven" (uses
    /// `cron_expr` and the caller-computed `next_run_ms`); any positive
    /// value picks the interval path, where `next_run` is recomputed as
    /// `now + interval_ms` at every fire (cron expression is ignored).
    #[allow(clippy::too_many_arguments)]
    async fn schedule_upsert_call(
        &self,
        name: &str,
        cron_expr: &str,
        kind: &str,
        payload: &serde_json::Value,
        priority: i16,
        max_attempts: i32,
        queue: &str,
        enabled: bool,
        next_run_ms: i64,
        interval_ms: i64,
    ) -> eddyq_core::Result<()> {
        let mut conn = self.conn_clone();
        let prefix = self.prefix.clone();
        let name = name.to_owned();
        let cron_expr = cron_expr.to_owned();
        let kind = kind.to_owned();
        let payload_s = payload.to_string();
        let priority_s = priority.to_string();
        let max_attempts_s = max_attempts.to_string();
        let queue = queue.to_owned();
        let enabled_s = if enabled { "1" } else { "0" }.to_owned();
        let next_run_s = next_run_ms.to_string();
        let interval_s = interval_ms.to_string();
        let _: redis::Value = with_retry(&mut conn, |mut c| {
            let prefix = prefix.clone();
            let name = name.clone();
            let cron_expr = cron_expr.clone();
            let kind = kind.clone();
            let payload_s = payload_s.clone();
            let priority_s = priority_s.clone();
            let max_attempts_s = max_attempts_s.clone();
            let queue = queue.clone();
            let enabled_s = enabled_s.clone();
            let next_run_s = next_run_s.clone();
            let interval_s = interval_s.clone();
            async move {
                redis::cmd("FCALL")
                    .arg(FN_SCHEDULE_UPSERT)
                    .arg(1)
                    .arg(&prefix)
                    .arg(&name)
                    .arg(&cron_expr)
                    .arg(&kind)
                    .arg(&payload_s)
                    .arg(&priority_s)
                    .arg(&max_attempts_s)
                    .arg(&queue)
                    .arg(&enabled_s)
                    .arg(&next_run_s)
                    .arg(&interval_s)
                    .query_async(&mut c)
                    .await
            }
        })
        .await?;
        Ok(())
    }

    /// Upsert an interval-driven schedule — fires every `interval_ms`
    /// independent of cron. Redis-only (the `Backend` trait surface stays
    /// cron-only; PG can grow this later via a schema column).
    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_interval_schedule_raw(
        &self,
        name: &str,
        interval_ms: i64,
        kind: &str,
        payload: serde_json::Value,
        priority: i16,
        max_attempts: i32,
        queue: &str,
    ) -> eddyq_core::Result<()> {
        if interval_ms <= 0 {
            return Err(eddyq_core::Error::InvalidArgument(
                "interval_ms must be > 0".into(),
            ));
        }
        // Initial next_run = now + interval. After each fire the leader loop
        // bumps it forward by `interval_ms` again.
        let next_run = self.now_ms() + interval_ms;
        self.schedule_upsert_call(
            name,
            "",
            kind,
            &payload,
            priority,
            max_attempts,
            queue,
            true,
            next_run,
            interval_ms,
        )
        .await
    }

    /// Read just the cron string for a stored schedule. Used by
    /// `set_schedule_enabled` to recompute `next_run_at` on re-enable.
    /// Returns `None` when the row is interval-driven (no cron to read).
    async fn lookup_cron(&self, name: &str) -> eddyq_core::Result<Option<String>> {
        let all = self.list_schedules().await?;
        Ok(all
            .into_iter()
            .find(|s| s.name == name)
            .and_then(|s| s.cron_expr))
    }

    async fn promote_delayed_call(&self, now_ms: i64) -> eddyq_core::Result<usize> {
        let mut conn = self.conn_clone();
        let prefix = self.prefix.clone();
        let now_ms = now_ms.to_string();
        let batch_max = "256".to_string();
        let value: redis::Value = with_retry(&mut conn, |mut c| {
            let prefix = prefix.clone();
            let now_ms = now_ms.clone();
            let batch_max = batch_max.clone();
            async move {
                redis::cmd("FCALL")
                    .arg(FN_PROMOTE_DELAYED)
                    .arg(1)
                    .arg(&prefix)
                    .arg(&now_ms)
                    .arg(&batch_max)
                    .query_async(&mut c)
                    .await
            }
        })
        .await?;
        Ok(value_to_u64(value) as usize)
    }

    /// One scheduler tick for cron schedules: list due names, expand each cron
    /// to compute next_run_at, then atomically fire (enqueue + advance).
    async fn fire_due_schedules(&self, now_ms: i64) -> eddyq_core::Result<usize> {
        let mut conn = self.conn_clone();
        let prefix = self.prefix.clone();
        let now_ms_s = now_ms.to_string();
        let batch_max = "128".to_string();
        let value: redis::Value = with_retry(&mut conn, |mut c| {
            let prefix = prefix.clone();
            let now_ms = now_ms_s.clone();
            let batch_max = batch_max.clone();
            async move {
                redis::cmd("FCALL")
                    .arg(FN_SCHEDULE_DUE_LIST)
                    .arg(1)
                    .arg(&prefix)
                    .arg(&now_ms)
                    .arg(&batch_max)
                    .query_async(&mut c)
                    .await
            }
        })
        .await?;
        let entries = match value {
            redis::Value::Array(a) => a,
            _ => return Ok(0),
        };
        let mut fired = 0usize;
        let mut iter = entries.into_iter();
        while let (Some(k), Some(v)) = (iter.next(), iter.next()) {
            let Some(name) = redis_to_string(&k) else {
                continue;
            };
            let Some(raw) = redis_to_string(&v) else {
                continue;
            };
            let Some(stored) = decode_stored(&name, &raw) else {
                continue;
            };
            // Interval > 0 → fixed-cadence schedule. Otherwise parse the
            // stored cron expression to compute the next run. Missing cron
            // on a non-interval row means the stored entry is malformed —
            // surface as a Cron error rather than panicking.
            let next_ms = if stored.interval_ms > 0 {
                now_ms + stored.interval_ms
            } else {
                let expr = stored.schedule.cron_expr.as_deref().ok_or_else(|| {
                    eddyq_core::Error::Cron(format!(
                        "schedule {:?} has no cron_expr and no interval_ms (malformed)",
                        stored.schedule.name
                    ))
                })?;
                next_run_ms_or_err(expr)?
            };
            self.schedule_fire_call(&stored.schedule, now_ms, next_ms)
                .await?;
            fired += 1;
        }
        Ok(fired)
    }

    async fn schedule_fire_call(
        &self,
        s: &Schedule,
        now_ms: i64,
        next_run_ms: i64,
    ) -> eddyq_core::Result<()> {
        let mut conn = self.conn_clone();
        let prefix = self.prefix.clone();
        let name = s.name.clone();
        let kind = s.kind.clone();
        let payload_s = s.payload.to_string();
        let priority_s = s.priority.to_string();
        let max_attempts_s = s.max_attempts.to_string();
        let queue = s.queue.clone();
        let now_s = now_ms.to_string();
        let next_s = next_run_ms.to_string();
        let _: redis::Value = with_retry(&mut conn, |mut c| {
            let prefix = prefix.clone();
            let name = name.clone();
            let kind = kind.clone();
            let payload_s = payload_s.clone();
            let priority_s = priority_s.clone();
            let max_attempts_s = max_attempts_s.clone();
            let queue = queue.clone();
            let now_s = now_s.clone();
            let next_s = next_s.clone();
            async move {
                redis::cmd("FCALL")
                    .arg(FN_SCHEDULE_FIRE)
                    .arg(1)
                    .arg(&prefix)
                    .arg(&name)
                    .arg(&kind)
                    .arg(&payload_s)
                    .arg(&priority_s)
                    .arg(&max_attempts_s)
                    .arg(&queue)
                    .arg(&now_s)
                    .arg(&next_s)
                    .query_async(&mut c)
                    .await
            }
        })
        .await?;
        Ok(())
    }
}

impl RedisBackend {
    /// Single-job lookup path for `list_jobs` when an id filter is set.
    async fn list_one_job(&self, id: JobId) -> eddyq_core::Result<JobList> {
        let mut conn = self.conn_clone();
        let key = format!("{}:job:{}", self.prefix, id);
        let h: std::collections::HashMap<String, String> = redis::cmd("HGETALL")
            .arg(&key)
            .query_async(&mut conn)
            .await
            .map_err(crate::Error::from)?;
        if h.is_empty() {
            return Ok(JobList {
                total: 0,
                rows: Vec::new(),
            });
        }
        let row = hash_to_job_row(id, &h);
        let rows = row.into_iter().collect();
        Ok(JobList { total: 1, rows })
    }
}

fn parse_rules_reply(value: redis::Value) -> Vec<StoredRule> {
    #[derive(serde::Deserialize)]
    struct RuleJson {
        max_concurrency: Option<i32>,
        rate_count: Option<i32>,
        rate_period_ms: Option<i32>,
        #[serde(default)]
        priority: i32,
    }
    let entries = match value {
        redis::Value::Array(a) => a,
        _ => return Vec::new(),
    };
    let now = chrono::Utc::now();
    let mut iter = entries.into_iter();
    let mut out = Vec::new();
    while let (Some(k), Some(v)) = (iter.next(), iter.next()) {
        let Some(pattern) = redis_to_string(&k) else {
            continue;
        };
        let Some(raw) = redis_to_string(&v) else {
            continue;
        };
        let Ok(r) = serde_json::from_str::<RuleJson>(&raw) else {
            continue;
        };
        out.push(StoredRule {
            pattern,
            max_concurrency: r.max_concurrency,
            rate_count: r.rate_count,
            rate_period_ms: r.rate_period_ms,
            priority: r.priority,
            created_at: now,
            updated_at: now,
        });
    }
    out.sort_by(|a, b| {
        b.priority
            .cmp(&a.priority)
            .then(b.pattern.len().cmp(&a.pattern.len()))
            .then(a.pattern.cmp(&b.pattern))
    });
    out
}

fn parse_stats_reply(value: redis::Value) -> JobStats {
    use eddyq_core::JobState;
    use eddyq_core::stats::QueueStateCount;
    let entries = match value {
        redis::Value::Array(a) => a,
        _ => return JobStats::default(),
    };
    let mut out = Vec::with_capacity(entries.len());
    for e in entries {
        let Some(s) = redis_to_string(&e) else {
            continue;
        };
        let mut parts = s.splitn(3, '|');
        let (Some(queue), Some(state), Some(count)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let parsed_state = match state {
            "pending" => Some(JobState::Pending),
            "running" => Some(JobState::Running),
            "scheduled" => Some(JobState::Scheduled),
            "completed" => Some(JobState::Completed),
            "failed" => Some(JobState::Failed),
            "cancelled" => Some(JobState::Cancelled),
            _ => None,
        };
        let parsed_count: i64 = count.parse().unwrap_or(0);
        if let Some(s) = parsed_state {
            out.push(QueueStateCount {
                queue: queue.to_string(),
                state: s,
                count: parsed_count,
            });
        }
    }
    JobStats {
        by_queue_state: out,
    }
}

fn parse_list_jobs_reply(value: redis::Value) -> eddyq_core::Result<(i64, Vec<JobId>)> {
    let arr = match value {
        redis::Value::Array(a) => a,
        _ => return Ok((0, Vec::new())),
    };
    let mut iter = arr.into_iter();
    let total: i64 = iter
        .next()
        .and_then(|v| redis_to_string(&v))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let mut ids = Vec::new();
    for v in iter {
        if let Some(s) = redis_to_string(&v) {
            if let Ok(id) = s.parse::<JobId>() {
                ids.push(id);
            }
        }
    }
    Ok((total, ids))
}

fn hash_to_job_row(
    id: JobId,
    h: &std::collections::HashMap<String, String>,
) -> Option<eddyq_core::stats::JobRow> {
    use eddyq_core::stats::JobRow;
    if h.is_empty() {
        return None;
    }
    let g = |k: &str| h.get(k).cloned();
    let ms_to_dt = |s: Option<String>| {
        s.and_then(|x| x.parse::<i64>().ok())
            .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis)
    };
    let priority: i16 = g("priority").and_then(|s| s.parse().ok()).unwrap_or(0);
    let attempt: i32 = g("attempt").and_then(|s| s.parse().ok()).unwrap_or(0);
    let max_attempts: i32 = g("max_attempts").and_then(|s| s.parse().ok()).unwrap_or(3);
    let scheduled_at = ms_to_dt(g("scheduled_at")).unwrap_or_else(chrono::Utc::now);
    let created_at = ms_to_dt(g("created_at")).unwrap_or_else(chrono::Utc::now);
    let finalized_at = ms_to_dt(g("completed_at"))
        .or_else(|| ms_to_dt(g("failed_at")))
        .or_else(|| ms_to_dt(g("cancelled_at")));
    let group_key = g("group_key").filter(|s| !s.is_empty());
    let tags: Vec<String> = g("tags")
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let payload: serde_json::Value = g("payload")
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or(serde_json::Value::Null);
    let result = g("result")
        .filter(|s| !s.is_empty())
        .and_then(|s| serde_json::from_str(&s).ok());
    let metadata: serde_json::Value = g("metadata")
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
    Some(JobRow {
        id,
        queue: g("queue").unwrap_or_default(),
        kind: g("kind").unwrap_or_default(),
        state: g("state").unwrap_or_default(),
        priority,
        attempt,
        max_attempts,
        scheduled_at,
        created_at,
        finalized_at,
        group_key,
        tags,
        payload,
        result,
        errors: serde_json::Value::Array(Vec::new()),
        metadata,
    })
}

/// Client-side filter application. The Lua side narrows by state+queue;
/// kind/tag/group_key are filtered here. Cheap because we cap rows at 500.
fn filter_matches(row: &eddyq_core::stats::JobRow, filter: &ListJobsFilter) -> bool {
    if let Some(k) = &filter.kind {
        if &row.kind != k {
            return false;
        }
    }
    if let Some(g) = &filter.group_key {
        if row.group_key.as_deref() != Some(g.as_str()) {
            return false;
        }
    }
    if let Some(t) = &filter.tag {
        if !row.tags.iter().any(|x| x == t) {
            return false;
        }
    }
    true
}

fn next_run_ms_or_err(cron_expr: &str) -> eddyq_core::Result<i64> {
    use std::str::FromStr;
    let schedule =
        cron::Schedule::from_str(cron_expr).map_err(|e| eddyq_core::Error::Cron(e.to_string()))?;
    let next = schedule
        .upcoming(chrono::Utc)
        .next()
        .ok_or_else(|| eddyq_core::Error::Cron("cron never fires".into()))?;
    Ok(next.timestamp_millis())
}

/// A Redis schedule plus the interval_ms hint used internally for fire
/// scheduling. The `Backend` trait's `Schedule` type doesn't carry interval
/// info (it's a Redis-only feature today) — we pair it here so the fire
/// loop knows whether to compute next_run from cron or from interval.
struct StoredSchedule {
    schedule: Schedule,
    interval_ms: i64,
}

fn decode_schedule_entry(name: &str, raw: &str) -> Option<Schedule> {
    decode_stored(name, raw).map(|s| s.schedule)
}

fn decode_stored(name: &str, raw: &str) -> Option<StoredSchedule> {
    #[derive(serde::Deserialize)]
    struct Stored {
        cron: String,
        kind: String,
        payload: String,
        priority: i16,
        max_attempts: i32,
        queue: String,
        enabled: i32,
        last_run_at_ms: i64,
        next_run_at_ms: i64,
        #[serde(default)]
        interval_ms: i64,
    }
    let stored: Stored = match serde_json::from_str(raw) {
        Ok(s) => s,
        Err(_) => return None,
    };
    let payload: serde_json::Value =
        serde_json::from_str(&stored.payload).unwrap_or(serde_json::Value::Null);
    // Map back to the cross-backend `Schedule` shape: cron and interval are
    // mutually exclusive triggers, so empty cron + positive interval_ms means
    // interval-driven (cron_expr=None, interval_ms=Some(N)); else cron-driven.
    let is_interval = stored.interval_ms > 0;
    let cron_expr = if is_interval || stored.cron.is_empty() {
        None
    } else {
        Some(stored.cron)
    };
    let core_interval = if is_interval {
        Some(stored.interval_ms)
    } else {
        None
    };
    Some(StoredSchedule {
        schedule: Schedule {
            name: name.to_owned(),
            kind: stored.kind,
            payload,
            cron_expr,
            interval_ms: core_interval,
            next_run_at: chrono::DateTime::<chrono::Utc>::from_timestamp_millis(
                stored.next_run_at_ms,
            )
            .unwrap_or_else(chrono::Utc::now),
            last_run_at: if stored.last_run_at_ms > 0 {
                chrono::DateTime::<chrono::Utc>::from_timestamp_millis(stored.last_run_at_ms)
            } else {
                None
            },
            enabled: stored.enabled == 1,
            priority: stored.priority,
            max_attempts: stored.max_attempts,
            queue: stored.queue,
        },
        interval_ms: stored.interval_ms,
    })
}

// Every `Backend` method now has a real impl — no remaining `Unsupported`
// stubs. (Helper removed after the last differentiator landed.)

// ============================================================
// FCALL helpers
// ============================================================

/// Wire-format `DynEnqueue` → 12 ARGV strings, in the order the Lua
/// `fn_enqueue` expects (see library.lua).
fn dyn_to_argv(req: &DynEnqueue) -> [String; 12] {
    let scheduled_at_ms = req.scheduled_at.map(|t| t.timestamp_millis()).unwrap_or(0);
    [
        req.kind.clone(),
        req.payload.to_string(),
        req.priority.to_string(),
        req.max_attempts.to_string(),
        scheduled_at_ms.to_string(),
        req.unique_key.clone().unwrap_or_default(),
        req.group_key.clone().unwrap_or_default(),
        req.queue.clone(),
        serde_json::to_string(&req.tags).unwrap_or_else(|_| "[]".into()),
        req.metadata.to_string(),
        // BullMQ-style per-job retention. Empty string = no rule; the
        // Lua `apply_retention` helper falls back to ZADDing the job into
        // the finalized ZSET so the queue-default `cleanup` tick can sweep it.
        RetentionRule::to_arg(req.remove_on_complete.as_ref()),
        RetentionRule::to_arg(req.remove_on_fail.as_ref()),
    ]
}

/// Parse the `eddyq_enqueue` return value `{ "inserted", "<id>" }` or
/// `{ "skipped" }` into an `EnqueueResult`.
fn parse_enqueue_reply(value: redis::Value) -> Result<EnqueueResult, crate::Error> {
    let arr = match value {
        redis::Value::Array(a) => a,
        other => {
            return Err(crate::Error::Protocol(format!(
                "expected array from enqueue, got {:?}",
                other
            )));
        }
    };
    let tag = match arr.first() {
        Some(redis::Value::BulkString(b)) => String::from_utf8_lossy(b).to_string(),
        Some(redis::Value::SimpleString(s)) => s.clone(),
        other => {
            return Err(crate::Error::Protocol(format!(
                "missing tag in enqueue reply: {:?}",
                other
            )));
        }
    };
    match tag.as_str() {
        "inserted" => {
            let id_val = arr
                .get(1)
                .ok_or_else(|| crate::Error::Protocol("missing id after 'inserted'".into()))?;
            let id_str = match id_val {
                redis::Value::BulkString(b) => String::from_utf8_lossy(b).to_string(),
                redis::Value::SimpleString(s) => s.clone(),
                redis::Value::Int(i) => i.to_string(),
                other => {
                    return Err(crate::Error::Protocol(format!(
                        "unexpected id shape: {:?}",
                        other
                    )));
                }
            };
            let id: i64 = id_str
                .parse()
                .map_err(|e| crate::Error::Protocol(format!("bad id {}: {}", id_str, e)))?;
            Ok(EnqueueResult::Inserted(id))
        }
        "skipped" => Ok(EnqueueResult::Skipped),
        other => Err(crate::Error::Protocol(format!(
            "unknown enqueue tag: {}",
            other
        ))),
    }
}

/// Each entry in `eddyq_claim`'s return is a JSON-encoded `ClaimedJob`.
fn parse_claim_reply(value: redis::Value) -> Result<Vec<ClaimedJob>, crate::Error> {
    let entries = match value {
        redis::Value::Array(a) => a,
        other => {
            return Err(crate::Error::Protocol(format!(
                "expected array from claim, got {:?}",
                other
            )));
        }
    };
    let mut out = Vec::with_capacity(entries.len());
    for e in entries {
        let s = match e {
            redis::Value::BulkString(b) => String::from_utf8_lossy(&b).to_string(),
            redis::Value::SimpleString(s) => s,
            other => {
                return Err(crate::Error::Protocol(format!(
                    "expected JSON string in claim reply, got {:?}",
                    other
                )));
            }
        };
        out.push(parse_claimed_job(&s)?);
    }
    Ok(out)
}

/// Turn the flat-array reply of `eddyq_group_get` into a `Group`. An empty
/// array means "no such group" — we surface that as `Ok(None)`.
fn parse_group_reply(value: redis::Value) -> Result<Option<Group>, crate::Error> {
    let entries = match value {
        redis::Value::Array(a) if !a.is_empty() => a,
        redis::Value::Array(_) => return Ok(None),
        other => {
            return Err(crate::Error::Protocol(format!(
                "expected array from group_get, got {:?}",
                other
            )));
        }
    };
    // Lua returns alternating ["key","<v>","field","<v>",...]. Walk pairs.
    let mut map: std::collections::HashMap<String, String> =
        std::collections::HashMap::with_capacity(entries.len() / 2);
    let mut iter = entries.into_iter();
    while let (Some(k), Some(v)) = (iter.next(), iter.next()) {
        let kk = redis_to_string(&k);
        let vv = redis_to_string(&v);
        if let (Some(kk), Some(vv)) = (kk, vv) {
            map.insert(kk, vv);
        }
    }
    let key = map.get("key").cloned().unwrap_or_default();
    let running_count: i32 = map
        .get("running_count")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let raw_max: i32 = map
        .get("max_concurrency")
        .and_then(|s| s.parse().ok())
        .unwrap_or(-1);
    let max_concurrency = if raw_max < 0 {
        eddyq_core::group::UNLIMITED
    } else {
        raw_max
    };
    let paused = map.get("paused").map(|s| s == "1").unwrap_or(false);
    let rate_raw: i32 = map
        .get("rate_count")
        .and_then(|s| s.parse().ok())
        .unwrap_or(-1);
    let rate_count = if rate_raw < 0 { None } else { Some(rate_raw) };
    let rate_period_ms = map
        .get("rate_period_ms")
        .and_then(|s| s.parse().ok())
        .filter(|n: &i32| *n > 0);
    let tokens: f64 = map
        .get("tokens")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.0);
    let refilled_at_ms: i64 = map
        .get("tokens_refilled_at_ms")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let tokens_refilled_at = if refilled_at_ms > 0 {
        chrono::DateTime::<chrono::Utc>::from_timestamp_millis(refilled_at_ms)
    } else {
        None
    };
    let now = chrono::Utc::now();
    Ok(Some(Group {
        key,
        running_count,
        max_concurrency,
        paused,
        rate_count,
        rate_period_ms,
        tokens,
        tokens_refilled_at,
        // We don't track these in Redis; surface "now" so the field is
        // present without misleading callers about prior values.
        created_at: now,
        updated_at: now,
    }))
}

/// Like `parse_group_reply` but for the named-queue admin shape.
fn parse_named_queue_reply(value: redis::Value) -> Result<Option<NamedQueue>, crate::Error> {
    let entries = match value {
        redis::Value::Array(a) if !a.is_empty() => a,
        redis::Value::Array(_) => return Ok(None),
        other => {
            return Err(crate::Error::Protocol(format!(
                "expected array from queue_get, got {:?}",
                other
            )));
        }
    };
    let mut map: std::collections::HashMap<String, String> =
        std::collections::HashMap::with_capacity(entries.len() / 2);
    let mut iter = entries.into_iter();
    while let (Some(k), Some(v)) = (iter.next(), iter.next()) {
        if let (Some(kk), Some(vv)) = (redis_to_string(&k), redis_to_string(&v)) {
            map.insert(kk, vv);
        }
    }
    let name = map.get("name").cloned().unwrap_or_default();
    let running_count: i32 = map
        .get("running_count")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let raw_max: i32 = map
        .get("max_concurrency")
        .and_then(|s| s.parse().ok())
        .unwrap_or(-1);
    let max_concurrency = if raw_max < 0 { i32::MAX } else { raw_max };
    let paused = map.get("paused").map(|s| s == "1").unwrap_or(false);
    let raw_timeout: i32 = map
        .get("default_timeout_ms")
        .and_then(|s| s.parse().ok())
        .unwrap_or(-1);
    let default_timeout_ms = if raw_timeout < 0 {
        None
    } else {
        Some(raw_timeout)
    };
    let now = chrono::Utc::now();
    Ok(Some(NamedQueue {
        name,
        running_count,
        max_concurrency,
        paused,
        default_timeout_ms,
        created_at: now,
        updated_at: now,
    }))
}

fn redis_to_string(v: &redis::Value) -> Option<String> {
    match v {
        redis::Value::BulkString(b) => Some(String::from_utf8_lossy(b).to_string()),
        redis::Value::SimpleString(s) => Some(s.clone()),
        redis::Value::Int(i) => Some(i.to_string()),
        _ => None,
    }
}

#[derive(serde::Deserialize)]
struct ClaimedJobJson {
    id: i64,
    kind: String,
    payload: String,
    #[allow(dead_code)]
    priority: i16,
    max_attempts: i32,
    attempt: i32,
    queue: String,
    #[serde(default)]
    group_key: Option<String>,
    worker_id: String,
}

fn parse_claimed_job(s: &str) -> Result<ClaimedJob, crate::Error> {
    let raw: ClaimedJobJson = serde_json::from_str(s)?;
    let payload: serde_json::Value = serde_json::from_str(&raw.payload)?;
    let worker_id = Uuid::parse_str(&raw.worker_id)
        .map_err(|e| crate::Error::Protocol(format!("bad worker_id: {}", e)))?;
    Ok(ClaimedJob {
        id: raw.id,
        kind: raw.kind,
        payload,
        attempt: raw.attempt,
        max_attempts: raw.max_attempts,
        group_key: raw.group_key,
        queue: raw.queue,
        worker_id,
        // Per-queue timeouts are wired in PR3 once named-queue admin lands
        // on Redis. Until then no Redis-claimed job has a timeout.
        timeout: None,
    })
}

#[async_trait]
impl Backend for RedisBackend {
    fn caps(&self) -> BackendCaps {
        BackendCaps {
            name: "redis",
            transactional_enqueue: false,
            migrations: false,
            fast_wakeup: true,
            // PR3 wires the cancel_requested heartbeat-poll. Until then
            // `cancel` only handles pending/scheduled jobs.
            cancel_running: false,
            // BullMQ uses (1, 2_097_152). Match for migration parity.
            priority_range: (1, 2_097_152),
            cluster_safe: true,
        }
    }

    // -------- enqueue ------------------------------------------------------

    async fn enqueue(&self, req: DynEnqueue) -> eddyq_core::Result<EnqueueResult> {
        let argv = dyn_to_argv(&req);
        let now = self.now_ms().to_string();
        let mut conn = self.conn_clone();
        let prefix = self.prefix.clone();

        let value = with_retry(&mut conn, |mut c| {
            let prefix = prefix.clone();
            let argv = argv.clone();
            let now = now.clone();
            async move {
                let mut cmd = redis::cmd("FCALL");
                cmd.arg(FN_ENQUEUE).arg(1).arg(&prefix);
                for a in &argv {
                    cmd.arg(a);
                }
                cmd.arg(&now);
                cmd.query_async::<redis::Value>(&mut c).await
            }
        })
        .await?;
        Ok(parse_enqueue_reply(value)?)
    }

    async fn enqueue_many(&self, reqs: Vec<DynEnqueue>) -> eddyq_core::Result<BulkEnqueueResult> {
        if reqs.is_empty() {
            return Ok(BulkEnqueueResult {
                inserted: 0,
                skipped: 0,
            });
        }
        // Build the flat ARGV: n, then n × 12 fields, then now_ms.
        let mut argv: Vec<String> = Vec::with_capacity(2 + reqs.len() * 12);
        argv.push(reqs.len().to_string());
        for r in &reqs {
            for a in dyn_to_argv(r) {
                argv.push(a);
            }
        }
        argv.push(self.now_ms().to_string());
        let mut conn = self.conn_clone();
        let prefix = self.prefix.clone();

        let value = with_retry(&mut conn, |mut c| {
            let prefix = prefix.clone();
            let argv = argv.clone();
            async move {
                let mut cmd = redis::cmd("FCALL");
                cmd.arg(FN_ENQUEUE_MANY).arg(1).arg(&prefix);
                for a in &argv {
                    cmd.arg(a);
                }
                cmd.query_async::<redis::Value>(&mut c).await
            }
        })
        .await?;

        // Lua returns flat array of pairs: ["inserted","<id>", "skipped","0", ...]
        let arr = match value {
            redis::Value::Array(a) => a,
            other => {
                return Err(crate::Error::Protocol(format!(
                    "expected array from enqueue_many, got {:?}",
                    other
                ))
                .into());
            }
        };
        let mut inserted = 0u64;
        let mut skipped = 0u64;
        let mut iter = arr.into_iter();
        while let (Some(tag), Some(_id)) = (iter.next(), iter.next()) {
            let s = match tag {
                redis::Value::BulkString(b) => String::from_utf8_lossy(&b).to_string(),
                redis::Value::SimpleString(s) => s,
                other => {
                    return Err(
                        crate::Error::Protocol(format!("unexpected tag: {:?}", other)).into(),
                    );
                }
            };
            match s.as_str() {
                "inserted" => inserted += 1,
                "skipped" => skipped += 1,
                _ => {}
            }
        }
        Ok(BulkEnqueueResult { inserted, skipped })
    }

    // -------- worker runtime hot path -------------------------------------

    async fn claim_batch(
        &self,
        worker_id: Uuid,
        batch_size: usize,
        kinds: &[String],
        queues: &[String],
    ) -> eddyq_core::Result<Vec<ClaimedJob>> {
        if batch_size == 0 || kinds.is_empty() || queues.is_empty() {
            return Ok(Vec::new());
        }
        let mut conn = self.conn_clone();
        let prefix = self.prefix.clone();
        let now_ms = self.now_ms().to_string();
        let worker_id_s = worker_id.to_string();
        let batch_s = batch_size.to_string();
        let nq = queues.len();
        let nk = kinds.len();
        let queues = queues.to_vec();
        let kinds = kinds.to_vec();

        let value = with_retry(&mut conn, |mut c| {
            let prefix = prefix.clone();
            let queues = queues.clone();
            let kinds = kinds.clone();
            let now_ms = now_ms.clone();
            let worker_id_s = worker_id_s.clone();
            let batch_s = batch_s.clone();
            async move {
                let mut cmd = redis::cmd("FCALL");
                cmd.arg(FN_CLAIM).arg(1).arg(&prefix);
                cmd.arg(&batch_s);
                cmd.arg(&worker_id_s);
                cmd.arg(&now_ms);
                cmd.arg("0"); // stale_lease_ms reserved arg
                cmd.arg(nq);
                for q in &queues {
                    cmd.arg(q);
                }
                cmd.arg(nk);
                for k in &kinds {
                    cmd.arg(k);
                }
                cmd.query_async::<redis::Value>(&mut c).await
            }
        })
        .await?;
        Ok(parse_claim_reply(value)?)
    }

    async fn update_heartbeat_batch(&self, ids: &[JobId]) -> eddyq_core::Result<u64> {
        if ids.is_empty() {
            return Ok(0);
        }
        let mut conn = self.conn_clone();
        let prefix = self.prefix.clone();
        let now_ms = self.now_ms().to_string();
        let id_strs: Vec<String> = ids.iter().map(|i| i.to_string()).collect();

        let value = with_retry(&mut conn, |mut c| {
            let prefix = prefix.clone();
            let now_ms = now_ms.clone();
            let id_strs = id_strs.clone();
            async move {
                let mut cmd = redis::cmd("FCALL");
                cmd.arg(FN_HEARTBEAT).arg(1).arg(&prefix).arg(&now_ms);
                for id in &id_strs {
                    cmd.arg(id);
                }
                cmd.query_async::<redis::Value>(&mut c).await
            }
        })
        .await?;
        Ok(value_to_u64(value))
    }

    async fn mark_completed(
        &self,
        id: JobId,
        worker_id: Uuid,
        result: Option<serde_json::Value>,
    ) -> eddyq_core::Result<()> {
        let mut conn = self.conn_clone();
        let prefix = self.prefix.clone();
        let id_s = id.to_string();
        let worker_id_s = worker_id.to_string();
        let now_ms = self.now_ms().to_string();
        let result_s = match result {
            Some(v) => v.to_string(),
            None => String::new(),
        };

        let value: redis::Value = with_retry(&mut conn, |mut c| {
            let prefix = prefix.clone();
            let id_s = id_s.clone();
            let worker_id_s = worker_id_s.clone();
            let now_ms = now_ms.clone();
            let result_s = result_s.clone();
            async move {
                redis::cmd("FCALL")
                    .arg(FN_COMPLETE)
                    .arg(1)
                    .arg(&prefix)
                    .arg(&id_s)
                    .arg(&worker_id_s)
                    .arg(&now_ms)
                    .arg(&result_s)
                    .query_async(&mut c)
                    .await
            }
        })
        .await?;
        if value_to_u64(value) == 0 {
            debug!(id, %worker_id, "complete: stale lease (no-op)");
        }
        Ok(())
    }

    async fn mark_failed(
        &self,
        id: JobId,
        worker_id: Uuid,
        error_entry: serde_json::Value,
        retry_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> eddyq_core::Result<()> {
        let mut conn = self.conn_clone();
        let prefix = self.prefix.clone();
        let id_s = id.to_string();
        let worker_id_s = worker_id.to_string();
        let now_ms = self.now_ms().to_string();
        let error_s = error_entry.to_string();
        let retry_at_ms = match retry_at {
            Some(t) => t.timestamp_millis().to_string(),
            None => "-1".to_string(),
        };

        let _: redis::Value = with_retry(&mut conn, |mut c| {
            let prefix = prefix.clone();
            let id_s = id_s.clone();
            let worker_id_s = worker_id_s.clone();
            let now_ms = now_ms.clone();
            let error_s = error_s.clone();
            let retry_at_ms = retry_at_ms.clone();
            async move {
                redis::cmd("FCALL")
                    .arg(FN_FAIL)
                    .arg(1)
                    .arg(&prefix)
                    .arg(&id_s)
                    .arg(&worker_id_s)
                    .arg(&now_ms)
                    .arg(&error_s)
                    .arg(&retry_at_ms)
                    .query_async(&mut c)
                    .await
            }
        })
        .await?;
        Ok(())
    }

    async fn sweep_stale(&self, stale_after: Duration) -> eddyq_core::Result<u64> {
        let mut conn = self.conn_clone();
        let prefix = self.prefix.clone();
        let cutoff_ms = (self.now_ms() - stale_after.as_millis() as i64).to_string();
        let batch_max = "256".to_string();

        let value: redis::Value = with_retry(&mut conn, |mut c| {
            let prefix = prefix.clone();
            let cutoff_ms = cutoff_ms.clone();
            let batch_max = batch_max.clone();
            async move {
                redis::cmd("FCALL")
                    .arg(FN_SWEEP_STALE)
                    .arg(1)
                    .arg(&prefix)
                    .arg(&cutoff_ms)
                    .arg(&batch_max)
                    .query_async(&mut c)
                    .await
            }
        })
        .await?;
        Ok(value_to_u64(value))
    }

    async fn cleanup(&self, retention: Retention) -> eddyq_core::Result<(u64, u64, u64, u64)> {
        // Bounded per call so a backlog can't block the Redis event loop —
        // the leader cleanup_loop ticks again on the next interval and
        // drains over many calls. Per-job retention already ran inline at
        // complete/fail time; this is only for jobs whose owner set no
        // per-job rule, or explicitly set `false`.
        const PER_STATE_LIMIT: u32 = 500;
        if retention.completed_secs.is_none()
            && retention.failed_secs.is_none()
            && retention.cancelled_secs.is_none()
            && retention.completed_count.is_none()
            && retention.failed_count.is_none()
            && retention.cancelled_count.is_none()
        {
            return Ok((0, 0, 0, 0));
        }
        let mut conn = self.conn_clone();
        let prefix = self.prefix.clone();
        let now_ms = self.now_ms().to_string();
        // -1 = skip this state; otherwise the configured age in seconds.
        // The Lua side distinguishes `0` ("sweep everything past now") from
        // `< 0` ("no rule configured for this state").
        let age_to_arg = |o: Option<u64>| match o {
            None => "-1".to_string(),
            Some(s) => i64::try_from(s).unwrap_or(i64::MAX).to_string(),
        };
        // -1 = no count cap; >= 0 means keep at most N newest. The Lua side
        // applies negative-index ZRANGE to pick victims beyond the top-N.
        let count_to_arg = |o: Option<i64>| match o {
            None => "-1".to_string(),
            Some(n) => n.max(0).to_string(),
        };
        let c = age_to_arg(retention.completed_secs);
        let f = age_to_arg(retention.failed_secs);
        let x = age_to_arg(retention.cancelled_secs);
        let cc = count_to_arg(retention.completed_count);
        let cf = count_to_arg(retention.failed_count);
        let cx = count_to_arg(retention.cancelled_count);
        let limit = PER_STATE_LIMIT.to_string();

        let value: redis::Value = with_retry(&mut conn, |mut conn| {
            let prefix = prefix.clone();
            let now_ms = now_ms.clone();
            let c = c.clone();
            let f = f.clone();
            let x = x.clone();
            let cc = cc.clone();
            let cf = cf.clone();
            let cx = cx.clone();
            let limit = limit.clone();
            async move {
                redis::cmd("FCALL")
                    .arg(FN_CLEANUP)
                    .arg(1)
                    .arg(&prefix)
                    .arg(&now_ms)
                    .arg(&c)
                    .arg(&f)
                    .arg(&x)
                    .arg(&cc)
                    .arg(&cf)
                    .arg(&cx)
                    .arg(&limit)
                    .query_async(&mut conn)
                    .await
            }
        })
        .await?;

        // Lua returns { n_completed, n_failed, n_cancelled, 0 } as bulk strings.
        // Reuse value_to_u64 element-wise; tolerate short replies defensively.
        let arr = match value {
            redis::Value::Array(a) => a,
            _ => return Ok((0, 0, 0, 0)),
        };
        let get = |i: usize| arr.get(i).cloned().map(value_to_u64).unwrap_or(0);
        Ok((get(0), get(1), get(2), get(3)))
    }

    async fn clean(
        &self,
        grace: Duration,
        limit: u32,
        state: CleanState,
    ) -> eddyq_core::Result<u64> {
        if limit == 0 {
            return Ok(0);
        }
        let mut conn = self.conn_clone();
        let prefix = self.prefix.clone();
        let now_ms = self.now_ms().to_string();
        let grace_secs = grace.as_secs().to_string();
        let limit_s = limit.to_string();
        // -1 on the two states we're not targeting so fn_cleanup skips them
        // (the targeted state gets the actual grace, which may be 0 meaning
        // "sweep everything past now"). `clean()` is the ad-hoc surface — no
        // count caps; the per-call cap is the batch_limit slot.
        let skip = "-1".to_string();
        let (c, f, x) = match state {
            CleanState::Completed => (grace_secs.clone(), skip.clone(), skip.clone()),
            CleanState::Failed => (skip.clone(), grace_secs.clone(), skip.clone()),
            CleanState::Cancelled => (skip.clone(), skip.clone(), grace_secs.clone()),
        };

        let value: redis::Value = with_retry(&mut conn, |mut conn| {
            let prefix = prefix.clone();
            let now_ms = now_ms.clone();
            let c = c.clone();
            let f = f.clone();
            let x = x.clone();
            let skip = skip.clone();
            let limit_s = limit_s.clone();
            async move {
                redis::cmd("FCALL")
                    .arg(FN_CLEANUP)
                    .arg(1)
                    .arg(&prefix)
                    .arg(&now_ms)
                    .arg(&c)
                    .arg(&f)
                    .arg(&x)
                    // No count cap on ad-hoc clean — limit is the batch cap.
                    .arg(&skip)
                    .arg(&skip)
                    .arg(&skip)
                    .arg(&limit_s)
                    .query_async(&mut conn)
                    .await
            }
        })
        .await?;

        let arr = match value {
            redis::Value::Array(a) => a,
            _ => return Ok(0),
        };
        let pick = match state {
            CleanState::Completed => 0,
            CleanState::Failed => 1,
            CleanState::Cancelled => 2,
        };
        Ok(arr.get(pick).cloned().map(value_to_u64).unwrap_or(0))
    }

    async fn reclaim_in_flight(&self, ids: &[JobId]) -> eddyq_core::Result<u64> {
        if ids.is_empty() {
            return Ok(0);
        }
        let mut conn = self.conn_clone();
        let prefix = self.prefix.clone();
        let now_ms = self.now_ms().to_string();
        let id_strs: Vec<String> = ids.iter().map(|i| i.to_string()).collect();

        let value: redis::Value = with_retry(&mut conn, |mut c| {
            let prefix = prefix.clone();
            let now_ms = now_ms.clone();
            let id_strs = id_strs.clone();
            async move {
                let mut cmd = redis::cmd("FCALL");
                cmd.arg(FN_RECLAIM_IN_FLIGHT)
                    .arg(1)
                    .arg(&prefix)
                    .arg(&now_ms);
                for id in &id_strs {
                    cmd.arg(id);
                }
                cmd.query_async(&mut c).await
            }
        })
        .await?;
        Ok(value_to_u64(value))
    }

    // -------- delayed promotion (called via schedule_tick on Redis) -------

    async fn schedule_tick(&self) -> eddyq_core::Result<usize> {
        // The scheduler loop is leader-gated and runs every
        // `scheduler_interval`. We do two things in this single tick:
        //   1. promote due delayed jobs (`fn_promote_delayed`)
        //   2. fire due cron schedules (`fn_schedule_due_list` + per-entry
        //      `fn_schedule_fire`, with cron expansion in Rust)
        // The cron-side mirrors the PG `schedule::tick` semantics — one
        // enqueue per tick with skip-missed.
        let now_ms = self.now_ms();
        let promoted = self.promote_delayed_call(now_ms).await?;
        let fired = self.fire_due_schedules(now_ms).await?;
        Ok(promoted + fired)
    }

    // -------- wakeup pubsub -----------------------------------------------

    fn spawn_wakeup_listener(
        self: Arc<Self>,
        wakeup: Arc<Notify>,
        shutdown: CancellationToken,
    ) -> Option<JoinHandle<()>> {
        let url = match build_url_for_pubsub(&self) {
            Ok(u) => u,
            Err(err) => {
                warn!(?err, "redis wakeup listener disabled (cannot derive URL)");
                return None;
            }
        };
        let channel = keys::wakeup_channel(&self.line);
        Some(tokio::spawn(async move {
            run_pubsub_listener(url, channel, "wakeup", wakeup, shutdown).await
        }))
    }

    // -------- leader -------------------------------------------------------

    async fn leader_try_elect(
        &self,
        worker_id: Uuid,
        role: &str,
        lease_secs: u64,
    ) -> eddyq_core::Result<bool> {
        let mut conn = self.conn_clone();
        let prefix = self.prefix.clone();
        let worker_id_s = worker_id.to_string();
        let lease_s = lease_secs.to_string();
        let now_ms = self.now_ms().to_string();
        let role = role.to_string();

        let value: redis::Value = with_retry(&mut conn, |mut c| {
            let prefix = prefix.clone();
            let worker_id_s = worker_id_s.clone();
            let lease_s = lease_s.clone();
            let now_ms = now_ms.clone();
            let role = role.clone();
            async move {
                redis::cmd("FCALL")
                    .arg(FN_LEADER_TRY)
                    .arg(1)
                    .arg(&prefix)
                    .arg(&worker_id_s)
                    .arg(&lease_s)
                    .arg(&now_ms)
                    .arg(&role)
                    .query_async(&mut c)
                    .await
            }
        })
        .await?;
        Ok(value_to_u64(value) == 1)
    }

    async fn leader_resign(&self, worker_id: Uuid, role: &str) -> eddyq_core::Result<()> {
        let mut conn = self.conn_clone();
        let prefix = self.prefix.clone();
        let worker_id_s = worker_id.to_string();
        let role = role.to_string();

        let _: redis::Value = with_retry(&mut conn, |mut c| {
            let prefix = prefix.clone();
            let worker_id_s = worker_id_s.clone();
            let role = role.clone();
            async move {
                redis::cmd("FCALL")
                    .arg(FN_LEADER_RESIGN)
                    .arg(1)
                    .arg(&prefix)
                    .arg(&worker_id_s)
                    .arg(&role)
                    .query_async(&mut c)
                    .await
            }
        })
        .await?;
        Ok(())
    }

    fn spawn_leader_resign_listener(
        self: Arc<Self>,
        on_resign: Arc<Notify>,
        shutdown: CancellationToken,
    ) -> Option<JoinHandle<()>> {
        let url = match build_url_for_pubsub(&self) {
            Ok(u) => u,
            Err(err) => {
                warn!(?err, "redis resign listener disabled");
                return None;
            }
        };
        let channel = keys::resign_channel(&self.line);
        Some(tokio::spawn(async move {
            run_pubsub_listener(url, channel, "resign", on_resign, shutdown).await
        }))
    }

    // -------- cancel -------------------------------------------------------

    async fn cancel(&self, id: JobId) -> eddyq_core::Result<bool> {
        let mut conn = self.conn_clone();
        let prefix = self.prefix.clone();
        let id_s = id.to_string();
        let now_ms = self.now_ms().to_string();

        let value: redis::Value = with_retry(&mut conn, |mut c| {
            let prefix = prefix.clone();
            let id_s = id_s.clone();
            let now_ms = now_ms.clone();
            async move {
                redis::cmd("FCALL")
                    .arg(FN_CANCEL)
                    .arg(1)
                    .arg(&prefix)
                    .arg(&id_s)
                    .arg(&now_ms)
                    .query_async(&mut c)
                    .await
            }
        })
        .await?;
        Ok(value_to_u64(value) == 1)
    }

    // -------- schedules / groups / named queues / read-only --------------
    //
    // Stubbed in PR2; full impls land in PR3. Returning Unsupported here is
    // safe for the runtime: scheduler/group/queue admin calls only happen
    // when the user explicitly invokes them or registers schedules/rules.

    async fn upsert_schedule_raw(
        &self,
        name: &str,
        cron_expr: &str,
        kind: &str,
        payload: serde_json::Value,
        priority: i16,
        max_attempts: i32,
        queue: &str,
    ) -> eddyq_core::Result<()> {
        let next_run = next_run_ms_or_err(cron_expr)?;
        self.schedule_upsert_call(
            name,
            cron_expr,
            kind,
            &payload,
            priority,
            max_attempts,
            queue,
            true,
            next_run,
            0, // interval_ms = 0 → cron-driven
        )
        .await
    }

    async fn upsert_interval_schedule_raw(
        &self,
        name: &str,
        interval_ms: i64,
        kind: &str,
        payload: serde_json::Value,
        priority: i16,
        max_attempts: i32,
        queue: &str,
    ) -> eddyq_core::Result<()> {
        // Delegate to the existing inherent method so there's one source
        // of truth for the validation + Lua call shape.
        RedisBackend::upsert_interval_schedule_raw(
            self,
            name,
            interval_ms,
            kind,
            payload,
            priority,
            max_attempts,
            queue,
        )
        .await
    }

    async fn remove_schedule(&self, name: &str) -> eddyq_core::Result<bool> {
        let mut conn = self.conn_clone();
        let prefix = self.prefix.clone();
        let name = name.to_owned();
        let value: redis::Value = with_retry(&mut conn, |mut c| {
            let prefix = prefix.clone();
            let name = name.clone();
            async move {
                redis::cmd("FCALL")
                    .arg(FN_SCHEDULE_REMOVE)
                    .arg(1)
                    .arg(&prefix)
                    .arg(&name)
                    .query_async(&mut c)
                    .await
            }
        })
        .await?;
        Ok(value_to_u64(value) > 0)
    }

    async fn set_schedule_enabled(&self, name: &str, enabled: bool) -> eddyq_core::Result<bool> {
        let mut conn = self.conn_clone();
        let prefix = self.prefix.clone();
        let name_owned = name.to_owned();
        let enabled_s = if enabled { "1" } else { "0" }.to_owned();
        // Need next_run when re-enabling. We don't know the cron from this
        // call alone, so look up the stored cron and compute it.
        let next_run = if enabled {
            match self.lookup_cron(&name_owned).await? {
                Some(cron) => next_run_ms_or_err(&cron)?.to_string(),
                None => return Ok(false),
            }
        } else {
            "0".to_string()
        };
        let enabled_s_arg = enabled_s.clone();
        let next_run_arg = next_run.clone();
        let value: redis::Value = with_retry(&mut conn, |mut c| {
            let prefix = prefix.clone();
            let name = name_owned.clone();
            let enabled_s = enabled_s_arg.clone();
            let next_run = next_run_arg.clone();
            async move {
                redis::cmd("FCALL")
                    .arg(FN_SCHEDULE_SET_ENABLED)
                    .arg(1)
                    .arg(&prefix)
                    .arg(&name)
                    .arg(&enabled_s)
                    .arg(&next_run)
                    .query_async(&mut c)
                    .await
            }
        })
        .await?;
        Ok(value_to_u64(value) > 0)
    }

    async fn list_schedules(&self) -> eddyq_core::Result<Vec<Schedule>> {
        let mut conn = self.conn_clone();
        let prefix = self.prefix.clone();
        let value: redis::Value = with_retry(&mut conn, |mut c| {
            let prefix = prefix.clone();
            async move {
                redis::cmd("FCALL")
                    .arg(FN_SCHEDULE_LIST)
                    .arg(1)
                    .arg(&prefix)
                    .query_async(&mut c)
                    .await
            }
        })
        .await?;
        let entries = match value {
            redis::Value::Array(a) => a,
            _ => return Ok(Vec::new()),
        };
        let mut out = Vec::with_capacity(entries.len() / 2);
        let mut iter = entries.into_iter();
        while let (Some(k), Some(v)) = (iter.next(), iter.next()) {
            let name = redis_to_string(&k).unwrap_or_default();
            let raw = redis_to_string(&v).unwrap_or_default();
            if let Some(s) = decode_schedule_entry(&name, &raw) {
                out.push(s);
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    async fn sync_schedules(
        &self,
        declared: &[ScheduleDeclaration],
    ) -> eddyq_core::Result<SyncReport> {
        // Pre-validate every cron + queue before mutating. Mirrors the PG impl
        // so a bad entry fails the whole sync (no partial state).
        let mut prepared = Vec::with_capacity(declared.len());
        for d in declared {
            let next = next_run_ms_or_err(&d.cron_expr).map_err(|e| match e {
                eddyq_core::Error::Cron(msg) => {
                    eddyq_core::Error::Cron(format!("{}: {}", d.name, msg))
                }
                other => other,
            })?;
            prepared.push((d, next));
        }
        for (d, next) in &prepared {
            self.schedule_upsert_call(
                &d.name,
                &d.cron_expr,
                &d.kind,
                &d.payload,
                d.priority,
                d.max_attempts,
                &d.queue,
                true,
                *next,
                0, // sync_schedules is always cron-driven; interval comes via the dedicated method
            )
            .await?;
        }
        // Diff out names no longer declared.
        let mut conn = self.conn_clone();
        let prefix = self.prefix.clone();
        let n = declared.len().to_string();
        let names: Vec<String> = declared.iter().map(|d| d.name.clone()).collect();
        let value: redis::Value = with_retry(&mut conn, |mut c| {
            let prefix = prefix.clone();
            let n = n.clone();
            let names = names.clone();
            async move {
                let mut cmd = redis::cmd("FCALL");
                cmd.arg(FN_SCHEDULE_SYNC_DIFF).arg(1).arg(&prefix).arg(&n);
                for nm in &names {
                    cmd.arg(nm);
                }
                cmd.query_async(&mut c).await
            }
        })
        .await?;
        let mut deleted = Vec::new();
        if let redis::Value::Array(arr) = value {
            for v in arr {
                if let Some(s) = redis_to_string(&v) {
                    deleted.push(s);
                }
            }
        }
        for name in &deleted {
            self.remove_schedule(name).await?;
        }
        Ok(SyncReport {
            upserted: declared.len(),
            deleted,
        })
    }
    async fn group_set_concurrency(&self, key: &str, max: i32) -> eddyq_core::Result<()> {
        let mut conn = self.conn_clone();
        let prefix = self.prefix.clone();
        let key = key.to_owned();
        let max = max.max(0).to_string();
        let _: redis::Value = with_retry(&mut conn, |mut c| {
            let prefix = prefix.clone();
            let key = key.clone();
            let max = max.clone();
            async move {
                redis::cmd("FCALL")
                    .arg(FN_GROUP_SET_CONCURRENCY)
                    .arg(1)
                    .arg(&prefix)
                    .arg(&key)
                    .arg(&max)
                    .query_async(&mut c)
                    .await
            }
        })
        .await?;
        Ok(())
    }

    async fn group_set_paused(&self, key: &str, paused: bool) -> eddyq_core::Result<()> {
        let mut conn = self.conn_clone();
        let prefix = self.prefix.clone();
        let key = key.to_owned();
        let p = if paused { "1" } else { "0" }.to_owned();
        let _: redis::Value = with_retry(&mut conn, |mut c| {
            let prefix = prefix.clone();
            let key = key.clone();
            let p = p.clone();
            async move {
                redis::cmd("FCALL")
                    .arg(FN_GROUP_SET_PAUSED)
                    .arg(1)
                    .arg(&prefix)
                    .arg(&key)
                    .arg(&p)
                    .query_async(&mut c)
                    .await
            }
        })
        .await?;
        Ok(())
    }

    async fn group_get(&self, key: &str) -> eddyq_core::Result<Option<Group>> {
        let mut conn = self.conn_clone();
        let prefix = self.prefix.clone();
        let key_owned = key.to_owned();
        let value: redis::Value = with_retry(&mut conn, |mut c| {
            let prefix = prefix.clone();
            let key = key_owned.clone();
            async move {
                redis::cmd("FCALL")
                    .arg(FN_GROUP_GET)
                    .arg(1)
                    .arg(&prefix)
                    .arg(&key)
                    .query_async(&mut c)
                    .await
            }
        })
        .await?;
        Ok(parse_group_reply(value)?)
    }

    async fn group_list(&self) -> eddyq_core::Result<Vec<Group>> {
        let mut conn = self.conn_clone();
        let prefix = self.prefix.clone();
        let value: redis::Value = with_retry(&mut conn, |mut c| {
            let prefix = prefix.clone();
            async move {
                redis::cmd("FCALL")
                    .arg(FN_GROUP_LIST)
                    .arg(1)
                    .arg(&prefix)
                    .query_async(&mut c)
                    .await
            }
        })
        .await?;
        let entries = match value {
            redis::Value::Array(a) => a,
            _ => return Ok(Vec::new()),
        };
        let mut out = Vec::with_capacity(entries.len());
        for e in entries {
            if let Some(g) = parse_group_reply(e)? {
                out.push(g);
            }
        }
        Ok(out)
    }

    async fn group_set_rate(
        &self,
        key: &str,
        count: u32,
        period: Duration,
    ) -> eddyq_core::Result<()> {
        let mut conn = self.conn_clone();
        let prefix = self.prefix.clone();
        let key = key.to_owned();
        let count_s = count.to_string();
        let period_ms = (period.as_millis() as u64).to_string();
        let now_ms = self.now_ms().to_string();
        let _: redis::Value = with_retry(&mut conn, |mut c| {
            let prefix = prefix.clone();
            let key = key.clone();
            let count_s = count_s.clone();
            let period_ms = period_ms.clone();
            let now_ms = now_ms.clone();
            async move {
                redis::cmd("FCALL")
                    .arg(FN_GROUP_SET_RATE)
                    .arg(1)
                    .arg(&prefix)
                    .arg(&key)
                    .arg(&count_s)
                    .arg(&period_ms)
                    .arg(&now_ms)
                    .query_async(&mut c)
                    .await
            }
        })
        .await?;
        Ok(())
    }

    async fn group_clear_rate(&self, key: &str) -> eddyq_core::Result<()> {
        let mut conn = self.conn_clone();
        let prefix = self.prefix.clone();
        let key = key.to_owned();
        let _: redis::Value = with_retry(&mut conn, |mut c| {
            let prefix = prefix.clone();
            let key = key.clone();
            async move {
                redis::cmd("FCALL")
                    .arg(FN_GROUP_CLEAR_RATE)
                    .arg(1)
                    .arg(&prefix)
                    .arg(&key)
                    .query_async(&mut c)
                    .await
            }
        })
        .await?;
        Ok(())
    }
    async fn group_set_rule(&self, pattern: &str, rule: GroupRule) -> eddyq_core::Result<()> {
        if rule.max_concurrency.is_none() && rule.rate_count.is_none() {
            return Err(eddyq_core::Error::InvalidArgument(
                "GroupRule must specify at least one of max_concurrency or rate".into(),
            ));
        }
        let rule_json = serde_json::json!({
            "max_concurrency": rule.max_concurrency,
            "rate_count": rule.rate_count,
            "rate_period_ms": rule
                .rate_period
                .map(|p| p.as_millis() as i64),
            "priority": rule.priority,
        });
        let mut conn = self.conn_clone();
        let prefix = self.prefix.clone();
        let pattern = pattern.to_owned();
        let json_s = rule_json.to_string();
        let _: redis::Value = with_retry(&mut conn, |mut c| {
            let prefix = prefix.clone();
            let pattern = pattern.clone();
            let json_s = json_s.clone();
            async move {
                redis::cmd("FCALL")
                    .arg(FN_GROUP_SET_RULE)
                    .arg(1)
                    .arg(&prefix)
                    .arg(&pattern)
                    .arg(&json_s)
                    .query_async(&mut c)
                    .await
            }
        })
        .await?;
        Ok(())
    }

    async fn group_remove_rule(&self, pattern: &str) -> eddyq_core::Result<bool> {
        let mut conn = self.conn_clone();
        let prefix = self.prefix.clone();
        let pattern = pattern.to_owned();
        let value: redis::Value = with_retry(&mut conn, |mut c| {
            let prefix = prefix.clone();
            let pattern = pattern.clone();
            async move {
                redis::cmd("FCALL")
                    .arg(FN_GROUP_REMOVE_RULE)
                    .arg(1)
                    .arg(&prefix)
                    .arg(&pattern)
                    .query_async(&mut c)
                    .await
            }
        })
        .await?;
        Ok(value_to_u64(value) > 0)
    }

    async fn group_list_rules(&self) -> eddyq_core::Result<Vec<StoredRule>> {
        let mut conn = self.conn_clone();
        let prefix = self.prefix.clone();
        let value: redis::Value = with_retry(&mut conn, |mut c| {
            let prefix = prefix.clone();
            async move {
                redis::cmd("FCALL")
                    .arg(FN_GROUP_LIST_RULES)
                    .arg(1)
                    .arg(&prefix)
                    .query_async(&mut c)
                    .await
            }
        })
        .await?;
        Ok(parse_rules_reply(value))
    }
    async fn queue_set_concurrency(&self, name: &str, max: i32) -> eddyq_core::Result<()> {
        let mut conn = self.conn_clone();
        let prefix = self.prefix.clone();
        let name = name.to_owned();
        let max = max.max(0).to_string();
        let _: redis::Value = with_retry(&mut conn, |mut c| {
            let prefix = prefix.clone();
            let name = name.clone();
            let max = max.clone();
            async move {
                redis::cmd("FCALL")
                    .arg(FN_QUEUE_SET_CONCURRENCY)
                    .arg(1)
                    .arg(&prefix)
                    .arg(&name)
                    .arg(&max)
                    .query_async(&mut c)
                    .await
            }
        })
        .await?;
        Ok(())
    }

    async fn queue_set_paused(&self, name: &str, paused: bool) -> eddyq_core::Result<()> {
        let mut conn = self.conn_clone();
        let prefix = self.prefix.clone();
        let name = name.to_owned();
        let p = if paused { "1" } else { "0" }.to_owned();
        let _: redis::Value = with_retry(&mut conn, |mut c| {
            let prefix = prefix.clone();
            let name = name.clone();
            let p = p.clone();
            async move {
                redis::cmd("FCALL")
                    .arg(FN_QUEUE_SET_PAUSED)
                    .arg(1)
                    .arg(&prefix)
                    .arg(&name)
                    .arg(&p)
                    .query_async(&mut c)
                    .await
            }
        })
        .await?;
        Ok(())
    }

    async fn queue_get(&self, name: &str) -> eddyq_core::Result<Option<NamedQueue>> {
        let mut conn = self.conn_clone();
        let prefix = self.prefix.clone();
        let name = name.to_owned();
        let value: redis::Value = with_retry(&mut conn, |mut c| {
            let prefix = prefix.clone();
            let name = name.clone();
            async move {
                redis::cmd("FCALL")
                    .arg(FN_QUEUE_GET)
                    .arg(1)
                    .arg(&prefix)
                    .arg(&name)
                    .query_async(&mut c)
                    .await
            }
        })
        .await?;
        Ok(parse_named_queue_reply(value)?)
    }

    async fn queue_list(&self) -> eddyq_core::Result<Vec<NamedQueue>> {
        let mut conn = self.conn_clone();
        let prefix = self.prefix.clone();
        let value: redis::Value = with_retry(&mut conn, |mut c| {
            let prefix = prefix.clone();
            async move {
                redis::cmd("FCALL")
                    .arg(FN_QUEUE_LIST)
                    .arg(1)
                    .arg(&prefix)
                    .query_async(&mut c)
                    .await
            }
        })
        .await?;
        let entries = match value {
            redis::Value::Array(a) => a,
            _ => return Ok(Vec::new()),
        };
        let mut out = Vec::with_capacity(entries.len());
        for e in entries {
            if let Some(nq) = parse_named_queue_reply(e)? {
                out.push(nq);
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    async fn queue_set_timeout(
        &self,
        name: &str,
        timeout: Option<Duration>,
    ) -> eddyq_core::Result<()> {
        let mut conn = self.conn_clone();
        let prefix = self.prefix.clone();
        let name = name.to_owned();
        let timeout_ms = match timeout {
            Some(d) => (d.as_millis() as i64).to_string(),
            None => "-1".to_string(),
        };
        let _: redis::Value = with_retry(&mut conn, |mut c| {
            let prefix = prefix.clone();
            let name = name.clone();
            let timeout_ms = timeout_ms.clone();
            async move {
                redis::cmd("FCALL")
                    .arg(FN_QUEUE_SET_TIMEOUT)
                    .arg(1)
                    .arg(&prefix)
                    .arg(&name)
                    .arg(&timeout_ms)
                    .query_async(&mut c)
                    .await
            }
        })
        .await?;
        Ok(())
    }
    async fn get_stats(&self) -> eddyq_core::Result<JobStats> {
        let mut conn = self.conn_clone();
        let prefix = self.prefix.clone();
        let value: redis::Value = with_retry(&mut conn, |mut c| {
            let prefix = prefix.clone();
            async move {
                redis::cmd("FCALL")
                    .arg(FN_GET_STATS)
                    .arg(1)
                    .arg(&prefix)
                    .query_async(&mut c)
                    .await
            }
        })
        .await?;
        Ok(parse_stats_reply(value))
    }

    async fn list_jobs(
        &self,
        filter: ListJobsFilter,
        pagination: Pagination,
    ) -> eddyq_core::Result<JobList> {
        // Special case: id filter → just HGETALL that one job, ignore other
        // filters (matches PG semantics where id is the strongest filter).
        if let Some(id) = filter.id {
            return self.list_one_job(id).await;
        }

        let state = match filter.state {
            Some(eddyq_core::JobState::Pending) => "pending",
            Some(eddyq_core::JobState::Running) => "running",
            Some(eddyq_core::JobState::Scheduled) => "scheduled",
            Some(eddyq_core::JobState::Completed) => "completed",
            Some(eddyq_core::JobState::Failed) => "failed",
            Some(eddyq_core::JobState::Cancelled) => "cancelled",
            None => "any",
        }
        .to_owned();
        let queue = filter.queue.clone().unwrap_or_default();
        let offset = pagination.offset.max(0).to_string();
        let limit = pagination.limit.clamp(1, 500).to_string();

        let mut conn = self.conn_clone();
        let prefix = self.prefix.clone();
        let value: redis::Value = with_retry(&mut conn, |mut c| {
            let prefix = prefix.clone();
            let state = state.clone();
            let queue = queue.clone();
            let offset = offset.clone();
            let limit = limit.clone();
            async move {
                redis::cmd("FCALL")
                    .arg(FN_LIST_JOBS)
                    .arg(1)
                    .arg(&prefix)
                    .arg(&state)
                    .arg(&queue)
                    .arg(&offset)
                    .arg(&limit)
                    .query_async(&mut c)
                    .await
            }
        })
        .await?;
        let (total, ids) = parse_list_jobs_reply(value)?;

        // HMGET each candidate. Pipelined for one round trip across all ids.
        let mut conn = self.conn_clone();
        let mut pipe = redis::pipe();
        for id in &ids {
            pipe.cmd("HGETALL")
                .arg(format!("{}:job:{}", self.prefix, id));
        }
        let hashes: Vec<std::collections::HashMap<String, String>> = if ids.is_empty() {
            Vec::new()
        } else {
            pipe.query_async(&mut conn)
                .await
                .map_err(crate::Error::from)?
        };

        let mut rows = Vec::with_capacity(hashes.len());
        for (id, h) in ids.iter().zip(hashes) {
            if let Some(row) = hash_to_job_row(*id, &h) {
                if filter_matches(&row, &filter) {
                    rows.push(row);
                }
            }
        }
        Ok(JobList { total, rows })
    }
}

// ============================================================
// Helpers
// ============================================================

fn value_to_u64(v: redis::Value) -> u64 {
    match v {
        redis::Value::Int(i) => i.max(0) as u64,
        redis::Value::BulkString(b) => String::from_utf8_lossy(&b).parse().unwrap_or(0),
        redis::Value::SimpleString(s) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

/// `ConnectionManager` doesn't expose its underlying URL, so the wakeup
/// listener can't re-derive a fresh PubSub-mode connection from it. For
/// PR2 we ask the operator to accept that pubsub uses a fresh connection
/// built from a known URL — stored on the `RedisBackend` next.
fn build_url_for_pubsub(_b: &RedisBackend) -> Result<String, crate::Error> {
    // ConnectionManager doesn't expose its URL. We'd need to plumb it
    // through `RedisConfig` -> `RedisBackend.url`. That's a one-line follow-up
    // — for now disable pubsub and rely on the fetcher's poll-floor.
    Err(crate::Error::Protocol(
        "wakeup pubsub disabled in PR2 (uses poll fallback)".into(),
    ))
}

#[allow(dead_code)]
async fn run_pubsub_listener(
    url: String,
    channel: String,
    label: &'static str,
    notify: Arc<Notify>,
    shutdown: CancellationToken,
) {
    let client = match redis::Client::open(url) {
        Ok(c) => c,
        Err(err) => {
            warn!(?err, label, "pubsub client open failed");
            return;
        }
    };
    let mut pubsub = match client.get_async_pubsub().await {
        Ok(p) => p,
        Err(err) => {
            warn!(?err, label, "pubsub connect failed");
            return;
        }
    };
    if let Err(err) = pubsub.subscribe(&channel).await {
        warn!(?err, label, channel = %channel, "pubsub subscribe failed");
        return;
    }
    info!(label, channel = %channel, "redis pubsub listener started");
    use futures_util::StreamExt;
    let mut stream = pubsub.on_message();
    loop {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => break,
            msg = stream.next() => match msg {
                Some(_) => notify.notify_one(),
                None => break,
            }
        }
    }
    info!(label, "redis pubsub listener stopped");
}
