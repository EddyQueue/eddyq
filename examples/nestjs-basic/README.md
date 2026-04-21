# @eddyq/example-nestjs-basic

A minimal NestJS app on top of `@eddyq/nestjs` showing the two patterns
every real queue-using app needs:

- **A controller that enqueues a job** → **a processor that handles it**
  (the `email/` module)
- **A service that registers a cron schedule** → **a processor that handles
  the scheduled job** (the `reports/` module)

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
├── email/                      # feature: controller enqueues, processor handles
│   ├── email.module.ts
│   ├── email.controller.ts     # POST /email/send → queue.enqueue("send.email", ...)
│   └── email.processor.ts      # @JobHandler("send.email")
│
└── reports/                    # feature: cron registration, processor handles
    ├── reports.module.ts
    ├── reports.service.ts      # onApplicationBootstrap → queue.addSchedule(...)
    └── reports.processor.ts    # @JobHandler("report.generate")
```

Both feature modules are imported by both composition roots. Controllers
are inert in the worker process (no HTTP listener), processors are inert
in the API process (`autoStart: false`). Keeping features whole like this
is cleaner than fragmenting `email.controller.ts` and `email.processor.ts`
into separate modules.

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
```

Watch the worker terminal — you'll see:

- `EmailProcessor` firing when the enqueued job is picked up
- `ReportsProcessor` firing every 10s from the cron registered by
  `ReportsService.onApplicationBootstrap`

## The patterns this shows

### 1. Feature module

Everything a feature needs lives in one folder. Both of these modules are
ordinary Nest modules — nothing eddyq-specific about the structure:

```ts
// email/email.module.ts
@Module({
  controllers: [EmailController],
  providers: [EmailProcessor],
})
export class EmailModule {}
```

### 2. Controller enqueues via `@InjectEddyq()`

```ts
// email/email.controller.ts
@Controller("email")
export class EmailController {
  constructor(@InjectEddyq() private readonly queue: Eddyq) {}

  @Post("send")
  async send(@Body() body: { to: string; subject: string }) {
    const r = await this.queue.enqueue("send.email", body, {
      uniqueKey: `email:${body.to}:${Date.now()}`,
    });
    return { jobId: r.id };
  }
}
```

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

### 4. Cron schedule registered at bootstrap

```ts
// reports/reports.service.ts
@Injectable()
export class ReportsService implements OnApplicationBootstrap {
  constructor(@InjectEddyq() private readonly queue: Eddyq) {}

  async onApplicationBootstrap() {
    // 6-field cron: `sec min hour dom month dow`
    await this.queue.addSchedule(
      "daily-report",
      "*/10 * * * * *",              // every 10s for demo; use "0 0 9 * * *" for 09:00 daily
      "report.generate",
      { scope: "daily" },
    );
  }
}
```

`addSchedule` is idempotent (upsert on `name`), so re-registering on every
boot is the intended pattern.

### 5. Two processes, one image

The same built code powers both entry points. In production you'd run a
small number of API pods (for HTTP) and a larger pool of worker pods (for
throughput), each process connected to the same Postgres. Split worker
fleets by queue when it matters:

```bash
# AI worker pool (uses a different queue): subscribe only to "ai"
EDDYQ_SUBSCRIBE_TO=ai  EDDYQ_WORKER_CONCURRENCY=1  pnpm start:worker

# Fast path: subscribe to the general queues
EDDYQ_SUBSCRIBE_TO=default  EDDYQ_WORKER_CONCURRENCY=40  pnpm start:worker
```
