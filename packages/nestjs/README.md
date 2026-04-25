# @eddyq/nestjs

NestJS module for [eddyq](https://github.com/eddyqueue/eddyq) — a Rust + Postgres
job queue with native Node bindings.

```
pnpm add @eddyq/queue @eddyq/nestjs
```

> npm has a long-standing bug with `optionalDependencies` + lockfiles that
> breaks packages shipping prebuilt binaries. **Use pnpm or yarn.**

## Quick start

```ts
// app.module.ts
import { Module } from "@nestjs/common";
import { EddyqModule } from "@eddyq/nestjs";
import { EmailProcessor } from "./email.processor";

@Module({
  imports: [
    EddyqModule.forRoot({
      databaseUrl: process.env.DATABASE_URL!,
      workerConcurrency: 20,
      subscribeTo: ["default", "urgent"],
    }),
  ],
  providers: [EmailProcessor],
})
export class AppModule {}
```

```ts
// email.processor.ts
import { Processor, JobHandler, type JobCall } from "@eddyq/nestjs";

@Processor()
export class EmailProcessor {
  @JobHandler("send.email")
  async send({ payload, id, attempt }: JobCall) {
    // throw to retry with exponential backoff.
    // throw new CancelError(...) to fail permanently.
    // throw new RetryError(..., { delayMs }) to retry at a specific delay.
    await sendgrid.send(payload);
  }
}
```

```ts
// some.controller.ts — enqueueing from a request handler
import { Controller, Post, Body } from "@nestjs/common";
import { InjectEddyq, type Eddyq } from "@eddyq/nestjs";

@Controller("notify")
export class NotifyController {
  constructor(@InjectEddyq() private readonly queue: Eddyq) {}

  @Post()
  async fanout(@Body() body: { to: string; subject: string }) {
    const r = await this.queue.enqueue("send.email", body, {
      uniqueKey: `${body.to}:${Date.now()}`,
      priority: 5,
    });
    return { jobId: r.id };
  }
}
```

That's the whole surface. Start Nest normally (`nest start`) — on
`onApplicationBootstrap` the module scans every provider for `@Processor()` +
`@JobHandler(kind)`, registers each as a worker, and starts the runtime.
On shutdown it drains in-flight jobs (default 30s grace) and closes the pool.

## Module configuration

### `forRoot(options)`

```ts
EddyqModule.forRoot({
  databaseUrl: "postgres://…",

  // Forwarded to `Eddyq.connect` — pool sizing + migration line.
  connectOptions: { maxConnections: 20, line: "main" },

  // Worker runtime (ignored if you have no @JobHandler providers).
  workerConcurrency: 20,            // default 10
  subscribeTo: ["default"],         // default ["default"]
  gracefulShutdownMs: 30_000,       // default 30_000

  // Lifecycle knobs.
  autoStart: true,                  // default true — false = register handlers only
  skipMigrationCheck: false,        // default false — match core's deploy-step guard
  runMigrations: false,             // default false — migrations are a deploy step

  // Cron schedules declared in code. Reconciled at boot; see "Cron schedules".
  schedules: [
    { name: "daily-report", cronExpr: "0 0 8 * * *", kind: "report.generate", payload: {} },
  ],
});
```

### `forRootAsync(options)` — for DI-sourced config

```ts
EddyqModule.forRootAsync({
  imports: [ConfigModule],
  inject: [ConfigService],
  useFactory: (cfg: ConfigService) => ({
    databaseUrl: cfg.getOrThrow("DATABASE_URL"),
    workerConcurrency: cfg.get<number>("QUEUE_CONCURRENCY") ?? 10,
  }),
});
```

## Error handling

Handlers can throw three things:

| Throw                                     | Effect                                                   |
| ----------------------------------------- | -------------------------------------------------------- |
| Any `Error`                               | Job retries with exponential backoff until `maxAttempts` |
| `new CancelError(msg)`                    | Mark failed permanently — no more retries                |
| `new RetryError(msg, { delayMs: 60_000 })` | Retry at a specific delay (e.g. honor `Retry-After`)     |

```ts
import { CancelError, RetryError } from "@eddyq/nestjs";

@JobHandler("webhook.call")
async call({ payload }: JobCall) {
  const r = await fetch(payload.url, { method: "POST" });
  if (r.status === 429) {
    const after = Number(r.headers.get("retry-after")) * 1000;
    throw new RetryError("rate limited", { delayMs: after });
  }
  if (r.status === 400) throw new CancelError("bad request — no retry");
  if (!r.ok) throw new Error(`HTTP ${r.status}`);
}
```

### Cooperative cancellation

On `app.close()` eddyq broadcasts `.abort()` to every in-flight handler.
Destructure `signal` off the `JobCall` and pass it to anything that accepts
an `AbortSignal`:

```ts
@JobHandler("download")
async download({ payload, signal }: JobCall) {
  const r = await fetch(payload.url, { signal });
  return await r.json();
}
```

## Admin, stats, and schedules

The raw `Eddyq` client is exported globally — inject it anywhere:

```ts
import { InjectEddyq, type Eddyq } from "@eddyq/nestjs";

@Injectable()
export class QueueAdmin {
  constructor(@InjectEddyq() private readonly q: Eddyq) {}

  async pauseIntegrations() {
    await this.q.pauseQueue("integrations");
  }

  async dashboard() {
    const [stats, queues, groups] = await Promise.all([
      this.q.getStats(),
      this.q.listNamedQueues(),
      this.q.listGroups(),
    ]);
    return { stats, queues, groups };
  }
}
```

See `@eddyq/queue` for the full method list: `enqueue`, `cancel`, `getStats`,
`listJobs`, `setGroupConcurrency`, `setGroupRate`, `setQueueConcurrency`,
`setQueueTimeout`, `addSchedule`, `removeSchedule`, and more.

## Cron schedules

Declare them in `forRoot`. The list is reconciled at boot — added entries are
upserted, removed ones are deleted. 6-field cron (`sec min hour dom month dow`).

```ts
import { EddyqModule, type ScheduleDeclaration } from "@eddyq/nestjs";

const schedules: ScheduleDeclaration[] = [
  { name: "daily-report", cronExpr: "0 0 8 * * *", kind: "report.generate", payload: { scope: "daily" } },
];

EddyqModule.forRoot({ databaseUrl: process.env.DATABASE_URL!, schedules });
```

Single-leader election with skip-missed semantics, so N replicas never
double-fire and a long outage doesn't replay every missed tick.

## Transactional enqueue

The main reason to use a Postgres-backed queue over Redis: your domain write
and your job enqueue commit atomically. If the commit fails, no invoice *and*
no follow-up job. If the job is queued, the invoice definitely exists.

eddyq exposes this via a **SQL function** (`eddyq_enqueue`) that runs inside
your own DB client's transaction — Prisma, Drizzle, Knex, pg, whatever you
already use:

```ts
// Prisma
await prisma.$transaction(async (tx) => {
  const invoice = await tx.invoice.create({ data });
  await tx.$executeRaw`
    SELECT eddyq_enqueue(
      'send.receipt',
      ${JSON.stringify({ invoiceId: invoice.id })}::jsonb
    )
  `;
});

// Drizzle
await db.transaction(async (tx) => {
  const [invoice] = await tx.insert(invoices).values(data).returning();
  await tx.execute(sql`
    SELECT eddyq_enqueue(
      'send.receipt',
      ${JSON.stringify({ invoiceId: invoice.id })}::jsonb
    )
  `);
});

// pg
await client.query("BEGIN");
const { rows } = await client.query("INSERT INTO invoices (...) RETURNING id", [...]);
await client.query(
  `SELECT eddyq_enqueue('send.receipt', $1::jsonb)`,
  [JSON.stringify({ invoiceId: rows[0].id })],
);
await client.query("COMMIT");
```

The full signature:

```sql
eddyq_enqueue(
  p_kind          text,
  p_payload       jsonb,
  p_queue         text        DEFAULT 'default',
  p_priority      smallint    DEFAULT 0,
  p_max_attempts  integer     DEFAULT 3,
  p_scheduled_at  timestamptz DEFAULT now(),
  p_unique_key    text        DEFAULT null,
  p_group_key     text        DEFAULT null,
  p_tags          text[]      DEFAULT '{}',
  p_metadata      jsonb       DEFAULT '{}'
) RETURNS bigint  -- the new job id, or NULL on unique_key conflict
```

And a bulk variant `eddyq_enqueue_many(jsonb)` taking a JSONB array of job
objects, returning `{ inserted: N, skipped: N }`.

The SQL path mirrors the Rust path exactly — same INSERT, same pattern-rule
materialization for `group_key`, same `pg_notify`. An integration test in
`eddyq-core` enqueues the same job via both paths and asserts identical rows,
so they can't drift.

**When to use which:**
- `this.queue.enqueue(...)` via `@InjectEddyq` — simple fire-and-forget, no
  outer transaction. The 90% case.
- `SELECT eddyq_enqueue(...)` inside your ORM's transaction — when the job
  must be atomic with a domain write.

## Migrations

Migrations are a **deploy-step concern**, not a runtime one. The default
`start()` path refuses to boot if any registered migration is unapplied — this
matches River's model and prevents rolling deploys from running workers
against a stale schema.

Three ways to apply migrations, in order of recommendation:

1. **A one-shot deploy job** (e.g. Kubernetes `Job`, ECS one-off task) that
   calls `Eddyq.connect(url).then((q) => q.migrate())` before workers roll.
2. The `eddyq` CLI: `eddyq migrate run --database-url $DATABASE_URL`.
3. **`runMigrations: true`** in `forRoot` — applies on bootstrap. Only use
   this for local dev or tests; it serializes every replica's boot behind
   schema migration.

Set `skipMigrationCheck: true` if you manage migrations out-of-band and want
to silence the boot-time guard.

## Requirements

- Node ≥ 20
- PostgreSQL ≥ 14
- `@nestjs/common` and `@nestjs/core` ^10 or ^11 (peer deps)
- `@eddyq/queue` same minor version (peer dep)

## License

MIT OR Apache-2.0
