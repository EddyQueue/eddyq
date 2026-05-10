# Architecture

How eddyq is wired internally. Audience: contributors and operators who need to reason about concurrency, failure modes, and scaling. For user-facing usage see `docs/`.

## Process model

A single Node worker process is a thin V8 shell wrapped around a multi-threaded Rust core. The Node event loop only runs your job handlers; every piece of queue plumbing — polling, leasing, heartbeats, sweeps, cron, retention, leader election — runs on real OS threads inside the NAPI-RS Tokio runtime.

```
┌──────────────────────────────────────────────────────────────────────────────────┐
│                          ONE NODE.JS WORKER PROCESS                               │
│                                                                                    │
│  ┌──────────────────────────────────┐   ┌──────────────────────────────────────┐  │
│  │   V8 / Node main thread          │   │  NAPI-RS Tokio runtime               │  │
│  │   (single-threaded event loop)   │   │  (multi-threaded; ~num_cpus OS thr.) │  │
│  │                                  │   │                                       │  │
│  │  ┌────────────────────────────┐  │   │  ┌─────────────────────────────────┐ │  │
│  │  │  User JS handlers          │  │   │  │ fetch_loop                      │ │  │
│  │  │  async ({payload}) => {…}  │◄─┼───┼──┤  runtime.rs:143                 │ │  │
│  │  │                            │  │   │  │  • claim_batch (FOR UPDATE      │ │  │
│  │  │  lib.cjs JS shim           │  │   │  │    SKIP LOCKED) — fetch.rs:42   │ │  │
│  │  │  • AbortController mgmt    │  │   │  │  • respects fetch_batch_size +  │ │  │
│  │  │  • [eddyq:err] envelope    │  │   │  │    channel capacity             │ │  │
│  │  └─────────────▲──────────────┘  │   │  └────────────┬────────────────────┘ │  │
│  │                │                 │   │               │ tx.send(ClaimedJob)  │  │
│  │                │ ThreadsafeFn    │   │               ▼                      │  │
│  │                │ call (NAPI hop) │   │      ┌─────────────────────┐        │  │
│  │                │ lib.rs:1084     │   │      │  mpsc channel       │        │  │
│  │                │                 │   │      │  capacity =         │        │  │
│  │                │                 │   │      │  worker_concurrency │        │  │
│  │                │                 │   │      └────────┬────────────┘        │  │
│  │                │                 │   │      Arc<Mutex<Receiver>>           │  │
│  │                │                 │   │               │                     │  │
│  │                │                 │   │   ┌───────────┼───────────┐         │  │
│  │                │                 │   │   ▼           ▼           ▼         │  │
│  │                │                 │   │  ┌──────┐  ┌──────┐  ┌──────┐      │  │
│  │                ╞═════════════════╪═══╪══┤ wkr0 │  │ wkr1 │…│ wkrN │      │  │
│  │                │   tsfn.call_    │   │  │loop  │  │loop  │  │loop  │      │  │
│  │                │   async(...)    │   │  └──┬───┘  └──┬───┘  └──┬───┘      │  │
│  │                │                 │   │     │ runtime.rs:217              │  │
│  │                │   Promise       │   │     │   • inserts job.id          │  │
│  │                ╞═════════════════╪═══╪═════│     into in_flight set      │  │
│  │                │   .await        │   │     │   • dispatcher() future     │  │
│  │                │                 │   │     │     calls tsfn.call_async   │  │
│  │                │                 │   │     │   • optional timeout wrap   │  │
│  │                │                 │   │     │   • catch_unwind            │  │
│  │                │                 │   │     │   • mark_completed/failed   │  │
│  │                │                 │   │     │   • removes from in_flight  │  │
│  │                │                 │   │     │                              │  │
│  │                │                 │   │  ┌──▼──────────────────────────┐  │  │
│  │                │                 │   │  │ shared in_flight: HashSet   │  │  │
│  │                │                 │   │  │ (Arc<Mutex>)                │  │  │
│  │                │                 │   │  └──┬──────────────────────────┘  │  │
│  │                │                 │   │     │                              │  │
│  │                │                 │   │  ┌──▼─────────────────────────┐   │  │
│  │                │                 │   │  │ heartbeat_loop             │   │  │
│  │                │                 │   │  │ runtime.rs:362             │   │  │
│  │                │                 │   │  │ • ONE batched UPDATE per   │   │  │
│  │                │                 │   │  │   heartbeat_interval       │   │  │
│  │                │                 │   │  │   regardless of #jobs      │   │  │
│  │                │                 │   │  └────────────────────────────┘   │  │
│  │                │                 │   │                                    │  │
│  │                │                 │   │  ┌────────────────────────────┐   │  │
│  │                │                 │   │  │ listener_loop              │   │  │
│  │                │                 │   │  │ runtime.rs:607             │   │  │
│  │                │                 │   │  │ • PgListener on            │   │  │
│  │                │                 │   │  │   "eddyq_job"              │   │  │
│  │                │                 │   │  │ • notify_one() → wakes     │   │  │
│  │                │                 │   │  │   fetch_loop               │   │  │
│  │                │                 │   │  └────────────────────────────┘   │  │
│  │                │                 │   │                                    │  │
│  │                │                 │   │  ┌────────────────────────────┐   │  │
│  │                │                 │   │  │ leader_loop  ← Atomic      │   │  │
│  │                │                 │   │  │ runtime.rs:394   is_leader │   │  │
│  │                │                 │   │  │ • try_elect every          │   │  │
│  │                │                 │   │  │   lease/3 secs             │   │  │
│  │                │                 │   │  │ • LISTEN leader_resign     │   │  │
│  │                │                 │   │  └─────────┬──────────────────┘   │  │
│  │                │                 │   │            │ gates ↓              │  │
│  │                │                 │   │  ┌─────────▼──────────────────┐   │  │
│  │                │                 │   │  │ sweeper_loop  (stale jobs) │   │  │
│  │                │                 │   │  │ scheduler_loop (cron tick) │   │  │
│  │                │                 │   │  │ cleanup_loop  (retention)  │   │  │
│  │                │                 │   │  │ — ONLY the leader runs     │   │  │
│  │                │                 │   │  │   these                    │   │  │
│  │                │                 │   │  └────────────────────────────┘   │  │
│  └──────────────────────────────────┘   └──────────────────────────────────────┘  │
│                                                       │                            │
└───────────────────────────────────────────────────────┼────────────────────────────┘
                                                        │ sqlx PgPool (small)
                                                        │ + 1 dedicated LISTEN socket
                                                        ▼
                                  ┌─────────────────────────────────────┐
                                  │            POSTGRES                 │
                                  │  eddyq_jobs, eddyq_groups,          │
                                  │  eddyq_queues, eddyq_schedules,     │
                                  │  eddyq_leader_lease, eddyq_batches  │
                                  │                                     │
                                  │  NOTIFY: eddyq_job, leader_resign   │
                                  └─────────────────────────────────────┘
```

The orange "═══" seam is the only place JS executes — the `ThreadsafeFunction` call in `crates/eddyq-napi/src/lib.rs:1084` hops to the Node main thread, runs the user handler, and returns a Promise the worker task awaits back on the Tokio side.

## Cluster topology

Multiple pods coordinate purely through Postgres. `FOR UPDATE SKIP LOCKED` (`crates/eddyq-core/src/fetch.rs:42`) guarantees no two pods claim the same job; one pod wins the maintenance lease and runs the singleton loops.

```
            POD A (leader)            POD B                   POD C
   ┌───────────────────────┐  ┌───────────────────────┐  ┌──────────────────┐
   │ Node proc             │  │ Node proc             │  │ Node proc        │
   │ ├─ fetcher            │  │ ├─ fetcher            │  │ ├─ fetcher       │
   │ ├─ N workers          │  │ ├─ N workers          │  │ ├─ N workers     │
   │ ├─ heartbeat          │  │ ├─ heartbeat          │  │ ├─ heartbeat     │
   │ ├─ listener           │  │ ├─ listener           │  │ ├─ listener      │
   │ ├─ leader_loop ★WON   │  │ ├─ leader_loop  lost  │  │ ├─ leader  lost  │
   │ ├─ sweeper   ACTIVE   │  │ ├─ sweeper   idle     │  │ ├─ sweeper  idle │
   │ ├─ scheduler ACTIVE   │  │ ├─ scheduler idle     │  │ ├─ scheduler idle│
   │ └─ cleanup   ACTIVE   │  │ └─ cleanup   idle     │  │ └─ cleanup  idle │
   └──────────┬────────────┘  └──────────┬────────────┘  └────────┬─────────┘
              │                          │                        │
              └──────────────┬───────────┴────────────────────────┘
                             ▼
                          Postgres
                FOR UPDATE SKIP LOCKED ensures
                no two pods claim the same job
```

When the leader shuts down gracefully it `NOTIFY`s `leader_resign` (`crates/eddyq-core/src/runtime.rs:394`), so a peer takes over within milliseconds instead of waiting for the lease to expire.

## Job lifecycle

One job, end to end. Numbers reference `crates/eddyq-core/src/runtime.rs` unless noted.

```
   enqueue (client)
        │
        │  INSERT eddyq_jobs (state='pending')
        │  + pg_notify('eddyq_job')
        ▼
   ┌───────────────┐
   │  Postgres     │
   │  pending row  │
   └──────┬────────┘
          │
          │ ① listener_loop:607 receives NOTIFY → wakeup.notify_one()
          ▼
   ┌──────────────┐
   │ fetch_loop   │ ② claim_batch — FOR UPDATE SKIP LOCKED
   │   :143       │    respects: queue caps, group caps, group rate limits,
   │              │    priority, scheduled_at; sets state='running'
   └──────┬───────┘
          │  ClaimedJob via mpsc channel (cap = worker_concurrency)
          ▼
   ┌──────────────┐
   │ worker_loop  │ ③ insert id into in_flight HashSet
   │   :217       │ ④ build dispatcher future
   │              │     ↳ tsfn.call_async(JobCall) — hop to Node
   │              │     ↳ user handler runs on V8
   │              │     ↳ Promise.await — back on Tokio
   │              │ ⑤ optional tokio::time::timeout wrap
   │              │ ⑥ catch_unwind around the whole thing
   └──────┬───────┘
          │
          ├─── Ok(value) ────────► mark_completed(state='completed', result=value)
          │
          ├─── Err(HandlerFailure) ─► retry_schedule(attempt, max_attempts)
          │      │                    ↳ if attempts left: state='pending',
          │      │                      scheduled_at = now + backoff
          │      │                    ↳ else: state='failed'
          │      │                    Directive::Cancel from JS skips retry.
          │      │                    Directive::Retry { delayMs } overrides backoff.
          │      ▼
          ├─── panic ─────────────► same as Err, name="Panic"
          │
          └─── timeout ───────────► same as Err, "job timed out after …"

   in parallel, while running:
       heartbeat_loop:362 — every heartbeat_interval, ONE batched
       UPDATE refreshes lease for ALL in_flight ids in this process.

   if a worker dies mid-job (lease goes stale > stale_after):
       sweeper_loop:505 (leader only) flips state back to 'pending'
       so another worker reclaims it on the next fetch.
```

## Why this layout

A few decisions that aren't obvious from reading any single file:

**Single fetcher, N workers, mpsc with capacity = concurrency** (`runtime.rs:50`). The fetcher only claims as many jobs as can fit in the channel, so we never lock rows we can't immediately work on. Backpressure is automatic: full channel ⇒ fetcher sleeps ⇒ rows stay `pending` for other pods.

**Workers share `Arc<Mutex<Receiver>>` instead of one channel per worker** (`runtime.rs:51`). Receiving is fast and contention is bounded by the fetch interval; the alternative (per-worker channels with a router) costs more code for no measurable throughput win.

**Single batched heartbeat instead of per-job** (`runtime.rs:362`). One `UPDATE … WHERE id = ANY($1)` per interval, regardless of in-flight count. This is the biggest silent advantage over per-job-lock systems: heartbeat cost is O(processes), not O(jobs).

**Leader gates maintenance, not job execution** (`runtime.rs:394`). Every pod claims jobs in parallel; only the leader runs sweeper / scheduler / cleanup. Losing leadership never blocks job throughput — it just means another pod takes over the singleton work.

**`spawn_blocking` + `Handle::block_on` in NAPI calls** (`crates/eddyq-napi/src/lib.rs:46`). sqlx's `Executor` impl isn't HRTB-`Send`-clean, so the `#[napi]` macro can't prove the futures `Send`-for-all-lifetimes. `spawn_blocking` only requires the closure to be `Send`, sidestepping the bound. Cost: one blocking-pool slot per admin call. Acceptable because admin calls are rare; hot-path job execution doesn't go through `run()`.

**Migrations are a deploy step, not a boot step** (`crates/eddyq-napi/src/lib.rs:900`). `start()` refuses to boot against a stale schema but never auto-applies — a slow migration would otherwise gate every replica's startup. Operators run `eddyq migrate` once per deploy.

## Failure modes

| Failure                          | What happens                                                                  |
| -------------------------------- | ----------------------------------------------------------------------------- |
| Worker process dies mid-job      | Heartbeat stops → sweeper (leader, `runtime.rs:505`) flips back to `pending`. |
| Leader dies                      | Lease expires; next pod's `try_elect` tick wins. Graceful shutdown `NOTIFY`s. |
| Postgres LISTEN connection drops | `listener_loop` exits; fetch falls back to `fetch_poll_interval`.             |
| Handler throws (sync)            | `catch_unwind` → marked failed with `name: "Panic"`.                          |
| Handler rejects (async)          | `[eddyq:err]` envelope parsed in `lib.rs:1109` → structured `HandlerFailure`. |
| Handler exceeds queue timeout    | `tokio::time::timeout` synthesizes a failure; retry plumbing handles it.     |
| `unique_key` conflict            | `EnqueueResult::Skipped`; no error.                                           |
| sqlx pool exhausted              | `acquire_timeout` returns; fetch loop logs and sleeps `fetch_cooldown`.       |

## Where to look in the code

| Concern              | File                                                       |
| -------------------- | ---------------------------------------------------------- |
| Process startup      | `crates/eddyq-core/src/runtime.rs` (`start`, line 43)      |
| Job claim SQL        | `crates/eddyq-core/src/fetch.rs` (`claim_batch`, line 42)  |
| Worker loop          | `crates/eddyq-core/src/runtime.rs` (`worker_loop`, line 217) |
| Heartbeat            | `crates/eddyq-core/src/runtime.rs` (`heartbeat_loop`, line 362) |
| Leader election      | `crates/eddyq-core/src/leader.rs` + `runtime.rs:394`       |
| Cron / scheduler     | `crates/eddyq-core/src/schedule.rs` + `runtime.rs:533`     |
| Retention / cleanup  | `crates/eddyq-core/src/fetch.rs` (`cleanup`) + `runtime.rs:561` |
| NAPI bridge          | `crates/eddyq-napi/src/lib.rs` (`dispatcher`, line 1064)   |
| JS shim              | `packages/queue/lib.cjs`                                    |
