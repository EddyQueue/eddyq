# Defining workers

A worker is a NestJS provider decorated with `@Processor()`. Each method that handles a job kind is annotated with `@JobHandler('kind')`.

```ts
import { Logger } from '@nestjs/common'
import { JobHandler, Processor, type JobCall } from '@eddyq/nestjs'

@Processor()
export class EmailProcessor {
  private readonly logger = new Logger(EmailProcessor.name)

  @JobHandler('send.email')
  async send({ payload, id }: JobCall) {
    const { to, subject } = payload as { to: string; subject: string }
    this.logger.log(`send.email #${id}: to=${to} subject="${subject}"`)
    // throw any Error to retry with exponential backoff
  }
}
```

Register the processor in the feature module — alongside the `registerQueue` call for the queue it consumes:

```ts
@Module({
  imports: [
    EddyqModule.registerQueue({ name: 'email', defaults: { maxAttempts: 5 } }),
  ],
  providers: [EmailProcessor],
})
export class EmailModule {}
```

The module's explorer scans every `@Processor()` class for `@JobHandler(kind)` methods and wires each one into the worker runtime at bootstrap.

## The `JobCall`

The handler receives a `JobCall` — the decoded job row plus an `AbortSignal` that fires on shutdown or cancellation:

```ts
@JobHandler('send.email')
async send({ payload, id, signal }: JobCall & { signal: AbortSignal }) {
  await this.mailer.send(payload, { signal })
}
```

Pass the signal into any I/O that supports cancellation. On shutdown the runtime aborts in-flight handlers and waits up to `gracefulShutdownMs` (default 30s) before forcing.

## Retries and cancellation

Throw `RetryError` or `CancelError` from `@eddyq/queue` — same semantics as the Node API.

```ts
import { RetryError, CancelError } from '@eddyq/queue'

@JobHandler('send.email')
async send({ payload }: JobCall) {
  if (rateLimited) throw new RetryError('429', { delayMs: 60_000 })
  if (badInput) throw new CancelError('invalid')
}
```

Any other thrown error retries with exponential backoff up to `maxAttempts` (set per queue under `registerQueue({ defaults })`, or per-enqueue).

## Concurrency and groups

`@JobHandler` doesn't take options — concurrency is process-wide (`forRoot.workerConcurrency`, default 10) and group behavior is configured on the queue:

```ts
EddyqModule.registerQueue({
  name: 'reports',
  groups: {
    'tenant-default': { concurrency: 4, rate: { count: 60, periodMs: 60_000 } },
  },
})
```

Then enqueue against a group key:

```ts
await this.reports.group(tenantId, 'tenant-default').enqueue('report.generate', payload)
```

See [Injecting a queue](./injecting#whats-on-the-handle) for the `group` sub-handle.
