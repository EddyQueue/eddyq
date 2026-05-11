# eddyq-redis

Redis Functions backend for [eddyq](../../README.md). Implements the
`eddyq_core::backend::Backend` trait against Redis 7+ — every hot-path
operation runs as an `FCALL` against the embedded `eddyq_v1` Lua library,
giving you transactional, single-round-trip semantics for enqueue / claim /
heartbeat / complete / fail without the operational drawbacks of legacy
`EVAL` + `EVALSHA`.

## What's in the library

`crates/eddyq-redis/src/functions/library.lua` registers `eddyq_v1` with
these functions:

| Function | Purpose |
|---|---|
| `eddyq_enqueue` | Single insert with unique-key dedup + group-rule materialization |
| `eddyq_enqueue_many` | Batched insert (one FCALL for N jobs) |
| `eddyq_claim` | Atomic batch fetch — kind/queue filter + group cap + rate-limit token bucket + named-queue cap, all in one Lua call |
| `eddyq_heartbeat` | Batched lease refresh |
| `eddyq_complete` / `eddyq_fail` | Terminate; honors per-job retention |
| `eddyq_sweep_stale` | Heartbeat sweep — reclaim stale leases |
| `eddyq_promote_delayed` | Move due delayed jobs into the wait set |
| `eddyq_reclaim_in_flight` | Force-shutdown reclaim |
| `eddyq_cancel` | Cancel a pending/scheduled job |
| `eddyq_leader_try` / `eddyq_leader_resign` | Leader election + resign |
| `eddyq_schedule_*` | Cron schedules + interval schedules + sync |
| `eddyq_group_*` | Group meta, concurrency cap, paused, token-bucket rate, pattern rules |
| `eddyq_queue_*` | Named-queue concurrency cap, pause, timeout |
| `eddyq_get_stats` / `eddyq_list_jobs` | Dashboard surface |

The library is `include_str!`'d into the binary and auto-loaded via
`FUNCTION LOAD REPLACE` on first FCALL. Loading is idempotent — peers
loading the same source byte-for-byte is a no-op; a SHA mismatch (a
deployed peer ships a newer library) triggers a replace. The load fans
out to AOF + replicas without you doing anything.

## Differentiators

Things you get for free on Redis that BullMQ Pro charges for:

- **Group concurrency caps + pause + token-bucket rate limits.** Atomically gated inside `eddyq_claim` — no race between cap-check and lease.
- **Pattern-based group rules.** `setGroupRule("tenant-*", { maxConcurrency: 5 })` and every new `tenant-acme`, `tenant-foo`, … gets the cap on first enqueue.
- **Cron schedules + `{ every: ms }` interval schedules.** Leader-fenced, skip-missed semantics.
- **Named-queue concurrency caps.** Cross-process limit per queue, not just per worker pool.
- **Per-job retention** (`removeOnComplete` / `removeOnFail`) — drop, keep N, keep age, or both.
- **`list_jobs` + `get_stats`.** Dashboard renders against Redis with the same shape as the Postgres backend.

## Cluster / hash-tag layout

Every key for a "line" is wrapped in `{<line>}` so all keys for one queue
map to a single Redis Cluster slot — multi-key Lua works inside one
function call. The default line is `"main"`; use distinct lines to
isolate workloads or to shard manually.

```
{main}:job:<id>              HASH    job row
{main}:job:<id>:errors       LIST    accumulated error log
{main}:wait:<queue>          ZSET    pending (priority desc, time asc)
{main}:delayed               ZSET    scheduled
{main}:active                ZSET    running (score = leased_at)
{main}:completed | :failed | :cancelled   ZSET    terminal states
{main}:queue:<q>             SET     job ids on queue <q> (filter index)
{main}:kind:<k>              SET     job ids of kind <k>
{main}:tag:<t>               SET     job ids tagged <t>
{main}:unique:<key>          STRING  dedup token (SET NX)
{main}:group:<key>:meta      HASH    max_concurrency, paused, rate_*
{main}:group:<key>:running   ZSET    running job ids in this group
{main}:groups                SET     group keys index
{main}:group_rules           HASH    pattern → rule JSON
{main}:nq:<q>:meta           HASH    named-queue admin meta
{main}:nq:<q>:running        ZSET    running per queue
{main}:schedules             HASH    name → entry JSON
{main}:schedules:next        ZSET    next_run_at_ms
{main}:leader:<role>         STRING  leader lease (SET NX EX)
```

## Caps + tradeoffs

```rust
BackendCaps {
    name: "redis",
    transactional_enqueue: false,   // no `enqueue_in_tx` — see Postgres backend
    migrations: false,              // library auto-loads, no schema
    fast_wakeup: true,              // pubsub fanout (poll-floor fallback)
    cancel_running: true,           // soft-cancel via HSET cancel_requested
    priority_range: (i16::MIN as i32, i16::MAX as i32),
    cluster_safe: true,             // one queue = one line = one slot
}
```

What's **not** here vs the Postgres backend:

- **Transactional enqueue** (`enqueue_in_tx`) — fundamentally PG-only.
- **Native batches** (`enqueue_batch` with fan-in callback) — the `eddyq_batches` fan-in table is PG-only. Use `enqueueMany` + an app-level counter on Redis.
- **AOF requirement for durability.** Schedules + cron rely on the leader's lease + the keyspace surviving restarts. If you run Redis without AOF and the node restarts, schedules will need to be re-registered by the app on boot.

## Wakeup

PubSub-based wakeup (`{<line>}:wakeup` channel) is wired but **disabled
by default** in this iteration — the runtime falls back to the fetcher's
poll-floor (`fetch_poll_interval`). The reason is that `redis::aio::ConnectionManager`
doesn't expose its URL, so the listener task can't re-derive a pubsub
connection without plumbing the URL through. Fix is a one-line change
in `build_url_for_pubsub` once we stash the URL on `RedisBackend`.

In practice, with `fetch_poll_interval: 50ms` you barely notice — the
poll fallback drains 18k+ jobs/sec at 64 workers. Pubsub is a latency
win for tail cases (a single rare job arriving in an otherwise-idle
queue).

## Usage from Rust

```rust
use eddyq_core::{Queue, QueueBuilder};
use eddyq_redis::{RedisBackend, RedisConfig};

let backend = RedisBackend::connect(RedisConfig {
    url: "redis://127.0.0.1:6379".into(),
    line: "main".into(),
}).await?;

let queue: Queue<RedisBackend> = QueueBuilder::with_backend(backend)
    .register::<SendEmail, _>(EmailWorker)
    .build();

queue.start()?;
queue.enqueue(&SendEmail { to: "alice@example.com".into() }).await?;
```

## Usage from Node

```ts
import { EddyqRedis } from "@eddyq/queue";

const queue = await EddyqRedis.connect("redis://…", { line: "main" });
queue.work("send-email", async ({ payload }) => { /* … */ });
await queue.start();

await queue.setGroupConcurrency("tenant-acme", 1);
await queue.addSchedule("cron-5min", { every: 5 * 60 * 1000 }, "WorkerJob.Cron5Min", {});
```

## Tests

```bash
docker compose -f ../../docker-compose.dev.yml up -d redis
REDIS_URL=redis://127.0.0.1:6381 cargo test -p eddyq-redis -- --test-threads=1
```

Currently 13 integration tests:
hot-path round-trip, bulk enqueue, unique-key dedup, group concurrency,
group pause, group rate limit, group rules (pattern materialization),
named-queue cap, schedule cron + sync, interval schedule, stats +
list_jobs.

## Benchmarks

See [`benches/README.md`](../../benches/README.md) for the throughput
harness — headline number is **~70k jobs/sec bulk ingest, ~19k jobs/sec
end-to-end drain at 64 workers**. The harness compares Redis vs Postgres
on identical workloads.

## Status

- ✅ Hot path: enqueue, enqueue_many, claim, heartbeat, complete, fail, sweep, reclaim, cancel, leader, promote_delayed
- ✅ Differentiators: groups (cap + pause + rate + rules), named-queue admin, schedules (cron + interval + sync), list_jobs, get_stats
- ✅ NAPI: `EddyqRedis` + `EddyqApp` (multi-backend container)
- ✅ NestJS: multi-backend `forRoot({ databaseUrl, redis, queues, defaultProvider })`
- 🚧 PubSub wakeup (disabled by default — see "Wakeup" above)
- 🚧 Cluster topology (single-line works; cross-line fan-out is application-level)

Not planned for this crate:
- `enqueue_in_tx` (PG-only by definition)
- `enqueue_batch` fan-in (PG-only — use `enqueueMany` + app-level counter)
