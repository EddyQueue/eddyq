# eddyq

> A Rust + Postgres job queue for the Node ecosystem.

**Status:** Beta. Running in production. APIs may change before 1.0 — pin exact versions.

## Why eddyq?

- **Postgres-native.** No new infrastructure. If you're already running Postgres, you can run eddyq.
- **Transactional enqueue.** Enqueue a job in the same transaction as your business write. No more "the job ran before the row committed" bugs.
- **First-class Node bindings.** `pnpm add @eddyq/queue` and ship from NestJS, Next.js, or any Node app.
- **Group concurrency.** Limit concurrent jobs per tenant, per provider, per anything.
- **Native batches.** Fan out N jobs and run a callback exactly once when they all settle — no per-app counter table.

## Batches

```ts
const { batchId } = await eddyq.enqueueBatch({
  items: shards.map((s) => ({ kind: "klaviyo.shard", payload: s })),
  onComplete: { kind: "klaviyo.attribution.recompute", payload: { integrationId } },
});
```

`onComplete` fires once when every item reaches a terminal state (success, terminal failure, or cancellation). The handler's payload gets a `_eddyq_batch` envelope with `{ batchId, total, completed, failed, cancelled, durationMs }` — branch on the counts to decide what success vs partial-failure means in your domain. End-to-end example: [`packages/queue/smoke-batch.mjs`](packages/queue/smoke-batch.mjs).

## Migrations are a deploy step

eddyq owns its own schema and ships migrations, but **they do not run
automatically at app boot**. Apply them via `eddyq migrate run` or a Node
one-shot script **before** starting workers. `eddyq.start()` refuses to boot
against a stale schema and tells you how to fix it. See the
[`@eddyq/queue` README](packages/queue/README.md#migrations--deploy-step-not-auto-apply)
for the rationale.

## Workspace layout

```
crates/
  eddyq-core/      # queue engine, schema, migrations
  eddyq-client/    # enqueue + admin API
  eddyq-cli/       # `eddyq` binary
  eddyq-napi/      # NAPI-RS Node bindings → @eddyq/queue
packages/
  queue/           # @eddyq/queue — TS wrapper
  nestjs/          # @eddyq/nestjs — NestJS module + decorators
  wakeboard/       # @eddyq/wakeboard — web UI (Svelte SPA + NestJS module)
benches/           # Criterion benchmarks for the queue engine
```

## License

- [MIT license](LICENSE-MIT)

at your option.

