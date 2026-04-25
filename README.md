# eddyq

> A Rust + Postgres job queue for the Node ecosystem.

**Status:** Pre-alpha. Under active development. APIs are unstable and the schema is not yet frozen.

## Why eddyq?

- **Postgres-native.** No new infrastructure. If you're already running Postgres, you can run eddyq.
- **Transactional enqueue.** Enqueue a job in the same transaction as your business write. No more "the job ran before the row committed" bugs.
- **First-class Node bindings.** `pnpm add @eddyq/queue` and ship from NestJS, Next.js, or any Node app.
- **Group concurrency.** Limit concurrent jobs per tenant, per provider, per anything.

## Migrations are a deploy step

eddyq owns its own schema and ships migrations, but **they do not run
automatically at app boot**. Apply them via `eddyq migrate run` or a Node
one-shot script **before** starting workers. `eddyq.start()` refuses to boot
against a stale schema and tells you how to fix it. See
[ADR 011](docs/decisions/011-migrations-deploy-step.md) and
[`@eddyq/queue` README](packages/queue/README.md#migrations--deploy-step-not-auto-apply).

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
  dashboard/       # web UI
docs/decisions/    # ADRs
benches/           # benchmark harness vs sqlxmq, graphile-worker, BullMQ
```

## License

- [MIT license](LICENSE-MIT)

at your option.

