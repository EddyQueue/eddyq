# What is eddyq

eddyq is a job queue for Node and NestJS that uses Postgres as its only dependency. The engine is written in Rust and exposed via [NAPI-RS](https://napi.rs) bindings, so the hot path stays predictable while the API stays familiar.

## Why eddyq?

- **Postgres-native.** No new infrastructure. If you already run Postgres, you can run eddyq.
- **Transactional enqueue.** Enqueue a job in the same transaction as your business write. No more "the job ran before the row committed" bugs.
- **First-class Node bindings.** `pnpm add @eddyq/queue` and ship from Express, Fastify, or any Node app. NestJS users get a [dedicated module](/nestjs/setup).
- **Group concurrency.** Limit concurrent jobs per tenant, per provider, per anything.
- **Predictable latency.** Rust core means GC pauses don't show up in your job throughput graphs.

## How it compares

| | eddyq | BullMQ | graphile-worker |
|---|---|---|---|
| Backend | Postgres | Redis | Postgres |
| Core language | Rust | Node | Node |
| Node bindings | First-class | Native | Native |
| NestJS module | Yes | Community | No |
| Transactional enqueue | ✅ | ❌ | ✅ |

## Next steps

- [**Installation**](./installation) — install the npm package and set up your database
- [**Migrations**](./migrations) — how schema changes work

Then pick your stack:

- [**Node.js quick start**](/node/quick-start) — Express, Fastify, or any Node app
- [**NestJS setup**](/nestjs/setup) — decorators, DI, and a `forRoot` module
