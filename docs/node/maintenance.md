# Maintenance & cleanup

Every running eddyq instance participates in **leader election** — exactly one process at a time runs the maintenance loops. No coordination required from your side; it just works.

The leader runs three background loops:

| Loop | Default interval | What it does |
|---|---|---|
| **Sweeper** | 30s | Recovers jobs whose worker died mid-execution (lease expired) |
| **Cleanup** | 5min | Deletes finalized jobs older than the per-state retention window |
| **Scheduler** | 5s | Inserts due jobs from your declared cron schedules |

## Sweeper — recovering stuck jobs

When a worker crashes (OOM, kernel panic, kubelet kill), its in-flight job is still marked `running` in Postgres. The job's lease (set when it was claimed, refreshed on heartbeat) eventually expires. The sweeper finds these abandoned jobs and returns them to `pending` so another worker can pick them up.

Default lease and heartbeat windows:

- **`heartbeat_interval`** — 15s. Workers refresh their lease this often while a handler is running.
- **`stale_after`** — 60s. A job is considered abandoned if no heartbeat in this window.

The 4× margin between heartbeat and stale window absorbs GC pauses, brief network blips, and slow Postgres writes.

## Cleanup — bounded history

Finalized jobs (`completed`, `failed`, `cancelled`) accumulate forever unless you delete them. The cleanup loop deletes them per-state once they age past the retention window.

Defaults:

| State | Default retention |
|---|---|
| `completed` | 24 hours |
| `failed` | 7 days |
| `cancelled` | 7 days |

Failed and cancelled jobs are kept longer because that's where you go to debug. Completed jobs go fast — most apps don't need a permanent log of every successful background task. If you do, set retention to `null` to keep forever, or copy rows to your own audit table from a job handler before they're cleaned up.

## Leader election

Leader election is a Postgres-native lease — one row, `SELECT FOR UPDATE SKIP LOCKED`. The default lease is **30 seconds**, refreshed every 10 seconds by the holder. If the leader dies, another process takes over within ~30s.

This means:

- **You don't need a separate "scheduler" pod.** Any worker can be the leader.
- **Maintenance survives deploys.** Mid-deploy, the new fleet takes over leadership.
- **Cleanup is single-writer.** No racing DELETEs across replicas.

## Tuning

Defaults are sensible for most workloads. Pass any of these to `start()` when you have a measured reason to deviate:

```ts
await q.start({
  sweepIntervalMs:        30_000,    // sweeper cadence
  staleAfterMs:           60_000,    // when a running job is considered abandoned
  heartbeatIntervalMs:    15_000,    // worker → DB heartbeat (must be ≪ staleAfterMs)
  cleanupIntervalMs:     300_000,    // retention sweep cadence
  completedRetentionSecs: 24 * 3600,
  failedRetentionSecs:    7 * 86400,
  cancelledRetentionSecs: 7 * 86400, // pass -1 to keep forever
  leaderLeaseSecs:        30,
  fetchPollIntervalMs:    1_000,     // ignored unless poll-only
});
```

When to touch them:

- **High throughput (millions of completed jobs/day)** — drop `completedRetentionSecs` to a few hours so cleanup keeps up.
- **Long handlers (jobs that legitimately run > 60s)** — raise `staleAfterMs` past your worst-case duration so the sweeper doesn't reclaim live jobs. Raise `heartbeatIntervalMs` proportionally.
- **Many replicas (50+ pods)** — raise `sweepIntervalMs` so you're not hammering the sweep query from every pod.

In NestJS, pass the same object as `tuning` on `EddyqModule.forRoot({ ... })`.
