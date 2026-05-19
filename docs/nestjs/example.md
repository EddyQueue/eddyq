# Example app

A complete, runnable NestJS app using `@eddyq/nestjs` lives at [`examples/nestjs-basic/`](https://github.com/EddyQueue/eddyq/tree/main/examples/nestjs-basic).

It demonstrates the patterns every queue-using app needs:

- **A feature module declares its queue with `registerQueue`, a controller enqueues via `@InjectQueue`, and a processor handles it** (the `email/` module)
- **A queue-scoped cron schedule fires a job that the same module's processor handles** (the `reports/` module)
- **Fan-in batches with an `onComplete` callback** (the `reports/run-shards` endpoint)

The app also splits **API** and **worker** entry points so they can scale independently.

## Project layout

```
src/
├── main.ts                 # API entry (NestFactory.create)
├── worker.ts               # Worker entry (createApplicationContext)
├── app.module.ts           # API root — autoStart: false
├── workers.module.ts       # Worker root — runs the job runtime
│
├── email/
│   ├── email.module.ts
│   ├── email.controller.ts # POST /email/send → queue.enqueue("send.email", ...)
│   └── email.processor.ts  # @JobHandler("send.email")
│
└── reports/
    ├── reports.module.ts
    ├── reports.controller.ts # POST /reports/run-shards → queue.enqueueBatch(...)
    └── reports.processor.ts  # @JobHandler("report.generate" | "report.shard" | "report.summary")
```

## API composition root

`autoStart: false` keeps the worker runtime off in the API process — API pods enqueue and serve admin reads, worker pods actually process jobs.

<<< ../../examples/nestjs-basic/src/app.module.ts{ts}

## Worker composition root

`createApplicationContext` boots the DI container without an HTTP listener — exactly what a worker pod needs.

<<< ../../examples/nestjs-basic/src/worker.ts{ts}

## Declaring a queue in a feature module

`EddyqModule.registerQueue` lives in the feature module that owns the queue. Per-queue `defaults` (and `schedules`, `groups`) ride along here — no central queue registry.

<<< ../../examples/nestjs-basic/src/email/email.module.ts{ts}

## Enqueueing from a controller

`@InjectQueue('email')` returns a `QueueHandle` pre-bound to the queue name + its defaults. Use `uniqueKey` to dedupe replays. `enqueueMany` is one round-trip for an entire batch.

<<< ../../examples/nestjs-basic/src/email/email.controller.ts{ts}

## Handling jobs

`@Processor()` marks the class. `@JobHandler('kind')` binds methods to job kinds. Throw any error to retry with exponential backoff; throw `CancelError` to give up; throw `RetryError` for an explicit delay.

<<< ../../examples/nestjs-basic/src/email/email.processor.ts{ts}

## Cron-scheduled jobs

Declare schedules on the owning queue via `registerQueue({ schedules })` — eddyq inserts the job at the right time, and your processor handles it like any other. The schedule's `queue` defaults to the enclosing `registerQueue` name.

<<< ../../examples/nestjs-basic/src/reports/reports.module.ts{ts}

<<< ../../examples/nestjs-basic/src/reports/reports.processor.ts{ts}

## Batches with a fan-in callback

`enqueueBatch` enqueues N items and fires a single callback when every item terminates. The reports controller injects the raw client with `@InjectEddyq()` because the batch spans multiple kinds — a `QueueHandle` works too when every item lands on the same queue. The summary handler reads counts under `_eddyq_batch` in its payload — see the [Batches page](/nestjs/batch) for the envelope shape.

<<< ../../examples/nestjs-basic/src/reports/reports.controller.ts{ts}

## Running it locally

```bash
docker compose -f docker-compose.dev.yml up -d
pnpm -C packages/queue build:debug
pnpm -C packages/nestjs build
pnpm -C examples/nestjs-basic dev:api      # http://localhost:3000
pnpm -C examples/nestjs-basic dev:worker   # picks up jobs
```

See the [example's README](https://github.com/EddyQueue/eddyq/tree/main/examples/nestjs-basic) for end-to-end test scripts (`load.mjs`).
