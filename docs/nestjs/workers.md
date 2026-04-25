# Defining workers

Workers are providers decorated with `@EddyqWorker`. Each method handler is annotated with `@EddyqProcess('job.name')`.

```ts
import { Injectable } from '@nestjs/common'
import { EddyqWorker, EddyqProcess, JobContext } from '@eddyq/nestjs'

@Injectable()
@EddyqWorker()
export class EmailWorker {
  constructor(private readonly mailer: MailerService) {}

  @EddyqProcess('send.email')
  async sendEmail(ctx: JobContext<{ to: string; body: string }>) {
    await this.mailer.send(ctx.payload.to, ctx.payload.body, { signal: ctx.signal })
  }
}
```

Register the worker in your module:

```ts
@Module({
  imports: [EddyqModule.forRoot({ databaseUrl })],
  providers: [EmailWorker],
})
export class EmailModule {}
```

## Concurrency and groups

Pass options to `@EddyqProcess`:

```ts
@EddyqProcess('send.email', {
  concurrency: 16,
  group: (ctx) => ctx.payload.tenantId,
  groupConcurrency: 4,
})
```

## Retries and cancellation

Throw `RetryError` or `CancelError` from `@eddyq/queue` — same semantics as the Node API.

```ts
import { RetryError, CancelError } from '@eddyq/queue'

@EddyqProcess('send.email')
async sendEmail(ctx: JobContext) {
  if (rateLimited) throw new RetryError('429', { delayMs: 60_000 })
  if (badInput) throw new CancelError('invalid')
}
```
