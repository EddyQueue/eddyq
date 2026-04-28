# @eddyq/queue

Node.js client for [eddyq](https://github.com/eddyqueue/eddyq) — a Rust + Postgres job queue.

## Install

```bash
pnpm add @eddyq/queue
```

> We recommend **pnpm** or **yarn** over npm — see the [npm caveat](https://github.com/eddyqueue/eddyq) for why NAPI-RS + npm has rough edges.

## Quick start

```ts
import { Eddyq, CancelError, RetryError } from "@eddyq/queue";

const q = await Eddyq.connect(process.env.DATABASE_URL);

q.work("send.email", async ({ payload, id, attempt, signal }) => {
  const res = await fetch(payload.url, { signal }); // signal flips on shutdown

  if (res.status === 429) {
    const after = Number(res.headers.get("retry-after") ?? 60) * 1000;
    throw new RetryError("rate limited", { delayMs: after });
  }
  if (!res.ok && attempt >= 3) {
    throw new CancelError("permanent 5xx; giving up");
  }

  return { bytes: (await res.text()).length };   // stored in eddyq_jobs.result
});

await q.start();

// ... your app runs ...

await q.shutdown(10_000);   // 10s grace; fires abort signals
```

## Migrations

eddyq does not auto-migrate at boot — slow migrations would stall every
replica's startup. Run them as an explicit deploy step before booting workers.

The package ships a CLI:

```bash
DATABASE_URL=postgres://... npx eddyq migrate run
DATABASE_URL=postgres://... npx eddyq migrate list
DATABASE_URL=postgres://... npx eddyq migrate down --max-steps 1 --confirm
```

Or call from code:

```ts
import { Eddyq } from '@eddyq/queue';
const q = await Eddyq.connect(process.env.DATABASE_URL);
const report = await q.migrate();
console.log("applied:", report.applied.map(m => `${m.version}:${m.name}`));
await q.close();
```

`eddyq.start()` refuses to boot if migrations are pending. To bypass when
schema is managed out-of-band, pass `{ skipMigrationCheck: true }`.

`migrate()` is idempotent and holds a `pg_advisory_lock` per migration line,
so running it twice (or from two deploy hosts at once) serializes safely.

## Retry convention

| Thrown | Behaviour |
|---|---|
| `new CancelError("reason")` | Permanent fail. No retry, regardless of `maxAttempts`. |
| `new RetryError("reason", { delayMs: 60_000 })` | Retry at now + `delayMs`. Overrides default backoff. |
| any other `Error` | Default exponential backoff, up to `maxAttempts`. |

Every failure persists the full `{ name, message, stack, directive, retryDelayMs }`
in `eddyq_jobs.errors[]`.

## Cancellation

Every handler receives `call.signal` — an `AbortSignal` that fires when
`eddyq.shutdown()` is called. Pass it along:

```ts
q.work("download", async ({ payload, signal }) => {
  const res = await fetch(payload.url, { signal });
  return res.json();
});
```

`shutdown(gracefulTimeoutMs?)` defaults to 30 000 ms. It broadcasts abort to
all in-flight handlers, waits up to the timeout, then force-cancels the
runtime tasks. Orphaned jobs are recovered by the heartbeat sweeper on the
next surviving worker.

## Admin surface

| Call | What it does |
|---|---|
| `enqueue(kind, payload, opts?)` | Enqueue a single job. `opts` covers `uniqueKey`, `priority`, `scheduledAtMs`, `groupKey`, `tags`, `metadata`, `maxAttempts`. |
| `cancel(jobId)` | Cancel a pending job. No-op on running/finalized. |
| `setGroupConcurrency(key, max)` | Cap concurrent jobs sharing `groupKey`. |
| `setGroupRate(key, count, periodMs)` | Token-bucket rate limit per group. |
| `pauseGroup / resumeGroup` | Pause dispatch for a group. |
| `setQueueConcurrency(queue, max)` | Cap total running across all worker processes. |
| `pauseQueue / resumeQueue` | Same, for a named queue. |
| `setQueueTimeout(queue, ms \| null)` | Default per-job timeout on this queue. |
| `addSchedule(name, cron, kind, payload, opts?)` | Upsert a cron schedule. 6-field cron (`sec min hour dom month dow`). `opts` covers `priority`, `maxAttempts`. |
| `syncSchedules(declared)` | Reconcile DB against a declared list — upserts each entry, deletes any schedule not in the list. Idempotent; the boot-time pattern when schedules are managed in code. |
| `removeSchedule(name)` / `setScheduleEnabled(name, enabled)` | Delete or toggle a schedule. |
| `listSchedules()` / `listNamedQueues()` / `listGroups()` | Read the admin state. |
| `getStats()` / `listJobs(filter?, pagination?)` | Dashboard-oriented reads. |
| `migrate()` / `migrateDown(n)` / `migrationStatus()` | Schema control. |

## Tuning

Defaults are sized for typical workloads — most users should leave them alone.
Pass any of these to `start()` when you need to deviate:

```ts
await q.start({
  sweepIntervalMs:        30_000,    // heartbeat sweeper cadence
  staleAfterMs:           60_000,    // when a running job is considered orphaned
  heartbeatIntervalMs:    15_000,    // worker → DB heartbeat (must be ≪ staleAfterMs)
  cleanupIntervalMs:     300_000,    // retention sweep cadence
  completedRetentionSecs: 24 * 3600, // delete completed older than this
  failedRetentionSecs:    7 * 86400,
  cancelledRetentionSecs: 7 * 86400, // pass -1 to keep forever
  leaderLeaseSecs:        30,        // single-leader maintenance lease
  fetchPollIntervalMs:    1_000,     // ignored unless poll-only
});
```

When to touch them:

- **High throughput (millions of completed jobs/day)** — drop
  `completedRetentionSecs` to a few hours so cleanup keeps up.
- **Long handlers (jobs that legitimately run > 60s)** — raise `staleAfterMs`
  past your worst-case duration so the sweeper doesn't reclaim live jobs.
  Also raise `heartbeatIntervalMs` proportionally.
- **Many replicas (50+ pods)** — raise `sweepIntervalMs` so you're not
  hammering the sweep query from every pod.

## License

MIT
