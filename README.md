# eddyq

> A Rust + Postgres job queue for the Node ecosystem.

**Status:** Pre-alpha. Under active development. APIs are unstable and the schema is not yet frozen.

## Why eddyq?

- **Postgres-native.** No new infrastructure. If you're already running Postgres, you can run eddyq.
- **Transactional enqueue.** Enqueue a job in the same transaction as your business write. No more "the job ran before the row committed" bugs.
- **First-class Node bindings.** `pnpm add @eddyq/queue` and ship from NestJS, Next.js, or any Node app.
- **River-grade Postgres semantics.** Hybrid LISTEN/NOTIFY + polling wakeup with jitter and debouncing. PgBouncer-aware deployment modes.
- **Group concurrency** (Phase 2). Limit concurrent jobs per tenant, per provider, per anything.

## Project status

eddyq is in **Phase 0** of its [v1.0 roadmap](docs/decisions/). The current focus is foundations: workspace, CI, ADRs, and a benchmark harness — *before* writing any queue logic, so design choices are bench-verified.

| Phase | Status |
|-------|--------|
| 0 — Foundations & benchmarks | 🟡 in progress |
| 1 — Core queue | ⏳ |
| 2 — Differentiators (groups, rate limits, transactional enqueue) | ⏳ |
| 3 — NAPI-RS Node bindings | ⏳ |
| 4 — Observability + dashboard | ⏳ |
| 5 — Production hardening | ⏳ |
| 6 — v1.0 launch | ⏳ |

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
  dashboard/       # web UI (Phase 4)
docs/decisions/    # ADRs
benches/           # benchmark harness vs sqlxmq, graphile-worker, BullMQ
```

## License

Dual-licensed under either of:

- [Apache License, Version 2.0](LICENSE-APACHE)
- [MIT license](LICENSE-MIT)

at your option.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Issues, PRs, and design discussions all welcome — but note this project is still in Phase 0 and the architecture is largely locked via [ADRs](docs/decisions/).
