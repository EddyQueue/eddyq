# @eddyq/queue

Node.js client for [eddyq](https://github.com/eddyqueue/eddyq) — a Rust + Postgres job queue.

**Pre-alpha.** APIs are unstable; the schema is not yet frozen.

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

## Migrations — DEPLOY STEP, not auto-apply

**eddyq does not run migrations at app boot.** Slow migrations (index builds,
table rewrites) would block every replica's startup. We follow River's /
Oban's / Prisma's discipline: **you apply migrations as an explicit deploy
step, then boot workers.**

`eddyq.start()` refuses to boot if migrations are pending and tells you how
to fix it. That's the safety net.

### The two ways to migrate

**1. Node script (good for Node-only deploys):**

```ts
// scripts/migrate.mjs — run this BEFORE booting workers.
import pkg from '@eddyq/queue';
const q = await pkg.Eddyq.connect(process.env.DATABASE_URL);
const report = await q.migrate();
console.log("applied:", report.applied.map(m => `${m.version}:${m.name}`));
await q.close();
```

Wire it into your deploy pipeline:

```yaml
# deploy.yml (sketch)
- run: node scripts/migrate.mjs
- run: systemctl restart workers.service
```

**2. CLI (good for non-Node / polyglot deploys):**

```bash
cargo install eddyq-cli   # or download binary
eddyq migrate run --database-url "$DATABASE_URL"
```

Both are idempotent. Both hold a `pg_advisory_lock` keyed per migration-line,
so running them twice (or from two deploy hosts simultaneously) serializes
safely instead of racing.

### Why not auto-migrate?

See [ADR 011](../../docs/decisions/011-migrations-deploy-step.md) for the
full reasoning. Short version:
- Slow migrations shouldn't block app boot.
- Enterprises want DDL reviewed.
- Every mature DB-backed queue (River, Oban, Prisma, Ecto) works this way.

### If you *really* know what you're doing

Skip the boot-time check:

```ts
await eddyq.start({ skipMigrationCheck: true });
```

For cases where schema is managed fully out-of-band and you've verified it's
in sync. Not the default.

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

## License

Dual-licensed MIT or Apache-2.0.
