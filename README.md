# eddyq

> A Rust job queue for the Node ecosystem — runs on Postgres, Redis, or both.

**Status:** Beta. Running in production. APIs may change before 1.0 — pin exact versions.

## Why eddyq?

- **Two backends, one API.** Postgres for transactional + durable; Redis (via Redis Functions, no Lua eval) for high-throughput ephemeral. Pick per-queue inside one app.
- **Transactional enqueue (Postgres).** Enqueue a job in the same transaction as your business write. No more "the job ran before the row committed" bugs.
- **High throughput (Redis).** ~70k jobs/sec bulk ingest, ~19k jobs/sec end-to-end drain at 64 workers — matches or beats BullMQ on the same hardware. See [`benches/README.md`](benches/README.md).
- **First-class Node bindings.** `pnpm add @eddyq/queue` and ship from NestJS, Next.js, or any Node app.
- **BullMQ Pro features, free, on either backend.** Group concurrency caps, token-bucket rate limits, pattern-based group rules, cron + `{ every: ms }` schedules, named-queue concurrency, per-job retention.
- **Native batches (Postgres).** Fan out N jobs and run a callback exactly once when they all settle — no per-app counter table.

## Pick a backend

| Workload | Backend | Why |
|---|---|---|
| Transactional enqueue (`enqueueInTx`), durable batches, strong audit trail | **Postgres** | One DB, ACID, no extra moving parts |
| Webhooks, fan-out, cache invalidation, anything ephemeral and hot | **Redis** | ~3–5× ingest, ~2–5× drain at scale; lower p50/p99 |
| Mixed (e.g. payments on Postgres, webhooks on Redis) | **Both** — `EddyqApp` | Per-queue routing in one app, single handler registry |

## Quick start (Postgres)

```ts
import { Eddyq } from "@eddyq/queue";

const queue = await Eddyq.connect("postgres://…");
await queue.migrate();                         // deploy step, not auto

queue.work("send-email", async ({ payload }) => { /* … */ });
await queue.start();
await queue.enqueue("send-email", { to: "alice@example.com" });
```

## Quick start (Redis)

```ts
import { EddyqRedis } from "@eddyq/queue";

const queue = await EddyqRedis.connect("redis://…");
// No migrations — the Redis Functions library auto-loads on first call.

queue.work("send-email", async ({ payload }) => { /* … */ });
await queue.start();
await queue.enqueue("send-email", { to: "alice@example.com" });

// BullMQ-style interval schedules:
await queue.addSchedule("cron-5min", { every: 5 * 60 * 1000 }, "WorkerJob.Cron5Min", {});
// Or a cron expression:
await queue.addSchedule("nightly", "0 0 0 * * *", "WorkerJob.Nightly", {});
```

## Multi-backend in one app

```ts
import { EddyqApp } from "@eddyq/queue";

const app = await EddyqApp.connect({
  postgres: { databaseUrl: "postgres://…" },
  redis:    { url: "redis://…", line: "main" },
  queues: [
    { name: "webhooks", provider: "redis" },     // hot, ephemeral
    { name: "payments", provider: "postgres" },  // transactional, durable
  ],
  defaultProvider: "postgres",
});

app.work("process", async ({ payload }) => { /* runs on either backend */ });
await app.start();

await app.enqueue("process", payload, { queue: "webhooks" });  // → Redis
await app.enqueue("process", payload, { queue: "payments" });  // → Postgres
```

NestJS users get the same wiring through `EddyqModule.forRoot`. See [`examples/nestjs-mixed/`](examples/nestjs-mixed/) for a full reference.

## Batches (Postgres)

```ts
const { batchId } = await eddyq.enqueueBatch({
  items: shards.map((s) => ({ kind: "klaviyo.shard", payload: s })),
  onComplete: { kind: "klaviyo.attribution.recompute", payload: { integrationId } },
});
```

`onComplete` fires once when every item reaches a terminal state (success, terminal failure, or cancellation). The handler's payload gets a `_eddyq_batch` envelope with `{ batchId, total, completed, failed, cancelled, durationMs }` — branch on the counts to decide what success vs partial-failure means in your domain. End-to-end example: [`packages/queue/smoke-batch.mjs`](packages/queue/smoke-batch.mjs).

> Batches are Postgres-only — the `eddyq_batches` table tracks fan-in state in a single transaction. On a Redis-routed queue, `enqueueBatch` throws. Use plain `enqueueMany` + an application-level counter when you need fan-in on Redis.

## Migrations are a deploy step

eddyq's **Postgres** backend owns its own schema and ships migrations, but **they do not run
automatically at app boot**. Apply them via `eddyq migrate run` or a Node
one-shot script **before** starting workers. `eddyq.start()` refuses to boot
against a stale schema and tells you how to fix it. See the
[`@eddyq/queue` README](packages/queue/README.md#migrations--deploy-step-not-auto-apply)
for the rationale.

The Redis backend has no migrations — the Redis Functions library (`eddyq_v1`) auto-loads on first call and replaces itself idempotently across rolling upgrades.

## Benchmarks

| Workload | Backend | bulk enqueue | end-to-end drain | single-enqueue p50 |
|---|---|---|---|---|
| 10k / 16 workers / batch 200 | Redis | **64,470/s** | 6,073/s | 139µs |
| 10k / 16 workers / batch 200 | Postgres | 51,521/s | 4,313/s | 554µs |
| 50k / 64 workers / batch 500 | Redis | 72,399/s | **19,219/s** | 149µs |

Full reproduction commands, knobs, and caveats: [`benches/README.md`](benches/README.md).

## Workspace layout

```
crates/
  eddyq-core/      # queue engine + Backend trait, schema, migrations
  eddyq-redis/    # Redis Functions backend (eddyq_v1 Lua library)
  eddyq-client/    # enqueue + admin API (Postgres)
  eddyq-cli/       # `eddyq` binary
  eddyq-napi/      # NAPI-RS Node bindings → @eddyq/queue
packages/
  queue/           # @eddyq/queue — TS wrapper (Eddyq, EddyqRedis, EddyqApp)
  nestjs/          # @eddyq/nestjs — NestJS module + decorators
  wakeboard/       # @eddyq/wakeboard — web UI (Svelte SPA + NestJS module)
benches/           # throughput harness + Criterion benches
examples/
  redis-basic/     # standalone Redis + multi-backend smoke
  nestjs-basic/    # Postgres-only Nest app
  nestjs-mixed/    # multi-backend Nest app (webhooks → Redis, payments → PG)
```

## License

- [MIT license](LICENSE-MIT)

at your option.
