# @eddyq/example-nestjs-basic

A minimal NestJS app on top of `@eddyq/nestjs` showing the patterns every
real queue-using app needs:

- **A feature module declares its queue with `registerQueue`** → **a
  controller enqueues via `@InjectQueue`** → **a processor handles it**
  (the `email/` module)
- **A cron schedule declared on the queue** → **the same module's
  processor handles the scheduled job** (the `reports/` module)
- **Fan-in batches with an `onComplete` callback** (the
  `reports/run-shards` endpoint)

Wrapped in the Nest feature-module pattern, with separate **API** and
**worker** entry points so the two can scale independently.

## Structure

```
src/
├── main.ts                     # API entry (NestFactory.create)
├── worker.ts                   # Worker entry (createApplicationContext)
├── app.module.ts               # API root — autoStart: false
├── workers.module.ts           # Worker root — runs the job runtime
│
├── email/                      # feature: enqueue, handle
│   ├── email.module.ts         # registerQueue({ name: "email", defaults })
│   ├── email.controller.ts     # @InjectQueue("email") → POST /email/send
│   └── email.processor.ts      # @JobHandler("send.email")
│
└── reports/                    # feature: scheduled job + fan-in batch
    ├── reports.module.ts       # registerQueue({ name: "reports", schedules })
    ├── reports.controller.ts   # POST /reports/run-shards (enqueueBatch)
    └── reports.processor.ts    # @JobHandler("report.generate" | "report.shard" | "report.summary")
```

Both feature modules are imported by both composition roots. Controllers
are inert in the worker process (no HTTP listener), processors are inert
in the API process (`autoStart: false`). The worker's `subscribeTo` list
is derived automatically from each feature's `registerQueue` — the
composition roots only own the connection and worker tuning.

## Run it

You need a running Postgres (the repo's `docker-compose.dev.yml` works):

```bash
# from the repo root
docker compose -f docker-compose.dev.yml up -d

# build workspace deps
pnpm -C packages/queue build:debug
pnpm -C packages/nestjs build

# build the example
pnpm -C examples/nestjs-basic build
```

In **two terminals**:

```bash
# terminal 1 — API
pnpm -C examples/nestjs-basic start:api

# terminal 2 — worker (with migrations on first run)
EDDYQ_RUN_MIGRATIONS=true pnpm -C examples/nestjs-basic start:worker
```

The API listens on `http://localhost:3000`. The worker has no listener.

## Try it

```bash
# Enqueue an email — the API responds immediately; the worker processes it.
curl -X POST http://localhost:3000/email/send \
  -H 'content-type: application/json' \
  -d '{"to":"rem@example.com","subject":"hello"}'

# Bulk enqueue — one Postgres round-trip for the whole batch. Useful for
# fan-out patterns (sending 500 emails after a signup import, etc.).
curl -X POST http://localhost:3000/email/send-bulk \
  -H 'content-type: application/json' \
  -d '{"messages":[
        {"to":"a@example.com","subject":"#1"},
        {"to":"b@example.com","subject":"#2"},
        {"to":"c@example.com","subject":"#3"}
      ]}'
# → { "inserted": 3, "skipped": 0 }

# Fan-in batch — enqueue N shards, fire one summary when all terminate.
curl -X POST http://localhost:3000/reports/run-shards \
  -H 'content-type: application/json' \
  -d '{"scope":"daily","shards":4}'
# → { "batchId": ..., "inserted": 4 }
```

Watch the worker terminal — you'll see:

- `EmailProcessor` firing when the enqueued job is picked up
- `EddyqModule` logging `synced schedules: upserted 1` on boot
- `ReportsProcessor` firing on the `daily-report` cron — change to `*/10 * * * * *` if you want to watch it locally
- `report.shard` firing N times then a single `report.summary`

## The patterns this shows

### 1. Feature module declares its queue

Everything a feature needs lives in one folder — including the
`registerQueue` call that declares its queue's defaults and schedules:

```ts
// email/email.module.ts
@Module({
  imports: [
    EddyqModule.registerQueue({
      name: "email",
      defaults: { maxAttempts: 5 },
    }),
  ],
  controllers: [EmailController],
  providers: [EmailProcessor],
})
export class EmailModule {}
```

### 2. Controller enqueues via `@InjectQueue(name)`

`@InjectQueue('email')` resolves a `QueueHandle` pre-bound to the queue
name and its `defaults` from `registerQueue`. Call sites don't restate
either.

```ts
// email/email.controller.ts
@Controller("email")
export class EmailController {
  constructor(@InjectQueue("email") private readonly queue: QueueHandle) {}

  @Post("send")
  async send(@Body() body: { to: string; subject: string }) {
    const r = await this.queue.enqueue("send.email", body, {
      uniqueKey: `email:${body.to}:${Date.now()}`,
    });
    return { jobId: r.id };
  }
}
```

When the target queue is dynamic (admin tools, generic dispatchers),
inject the raw client with `@InjectEddyq()` instead — that's what
`reports.controller.ts` does for `enqueueBatch`, since the shard items
and the summary callback are different kinds.

### 3. Processor with `@JobHandler(kind)`

```ts
// email/email.processor.ts
@Processor()
export class EmailProcessor {
  @JobHandler("send.email")
  async send({ payload, id }: JobCall) {
    // do the work. throw to retry, throw CancelError to fail permanently,
    // throw RetryError to retry at a specific delay.
  }
}
```

### 4. Cron schedules declared on the queue

Schedules belong to the queue they fire onto. Declare them on the same
`registerQueue` call — `queue` defaults to the enclosing name.

```ts
// reports/reports.module.ts
EddyqModule.registerQueue({
  name: "reports",
  defaults: { priority: 5 },
  schedules: [
    {
      name: "daily-report",
      cronExpr: "0 0 8 * * *",
      kind: "report.generate",
      payload: { scope: "daily" },
    },
  ],
})
```

The DB schedule table is reconciled at boot against the union of every
`registerQueue({ schedules })` and `forRoot.schedules` — added entries
are upserted, removed ones deleted.

### 5. Two processes, one image — auto-derived subscriptions

The same built code powers both entry points. The worker's `subscribeTo`
list is derived automatically from each feature's `registerQueue` — no
central queue registry to keep in sync. In production you'd run a small
number of API pods and a larger pool of worker pods, each connected to
the same Postgres. Override `subscribeTo` per-fleet to split work:

```bash
# AI worker pool (only handles "ai" jobs)
EDDYQ_SUBSCRIBE_TO=ai  EDDYQ_WORKER_CONCURRENCY=1  pnpm start:worker

# Fast path: subscribe to the general queues
EDDYQ_SUBSCRIBE_TO=email,reports  EDDYQ_WORKER_CONCURRENCY=40  pnpm start:worker
```
