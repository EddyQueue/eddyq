# Injecting a queue

Each queue you've declared with [`EddyqModule.registerQueue`](./setup#registering-a-queue) gets a typed handle in DI. Inject it with `@InjectQueue(name)`:

```ts
import { Injectable } from '@nestjs/common'
import { InjectQueue, type QueueHandle } from '@eddyq/nestjs'

@Injectable()
export class SignupService {
  constructor(@InjectQueue('email') private readonly email: QueueHandle) {}

  async signup(address: string) {
    const user = await this.users.create({ address })
    await this.email.enqueue('send.email', {
      to: address,
      template: 'welcome',
    })
    return user
  }
}
```

The handle binds the queue name and the `defaults` from `registerQueue`, so call sites don't repeat them. Caller-supplied options still win on conflict.

## What's on the handle

`QueueHandle` exposes a queue-scoped enqueue surface:

- `enqueue(kind, payload, options?)` — one job. `options` is the usual `EnqueueOptions` minus `queue` (bound by the handle).
- `enqueueMany(items)` — bulk enqueue, one Postgres round-trip.
- `enqueueBatch({ items, onComplete?, metadata? })` — fan-in batch with an optional callback when every item terminates.
- `group(groupKey, profile)` — sub-handle pre-bound to a group key. `profile` can be a profile name from `registerQueue({ groups })` or an inline `{ concurrency, rate }`.

```ts
@InjectQueue('reports') private readonly reports: QueueHandle

await this.reports
  .group(tenantId, 'tenant-default')
  .enqueue('report.generate', { scope: 'daily' })
```

## Transactional enqueue

Pass a transaction client as `tx` — the job becomes visible only when the surrounding transaction commits.

```ts
@Injectable()
export class SignupService {
  constructor(
    @InjectQueue('email') private readonly email: QueueHandle,
    private readonly db: DbService,
  ) {}

  async signup(address: string) {
    return this.db.tx(async (tx) => {
      const user = await tx.users.create({ address })
      await this.email.enqueue('send.email', { userId: user.id }, { tx })
      return user
    })
  }
}
```

## From a controller

```ts
@Controller('webhooks')
export class WebhookController {
  constructor(@InjectQueue('webhooks') private readonly queue: QueueHandle) {}

  @Post('stripe')
  async handle(@Body() event: StripeEvent) {
    await this.queue.enqueue('stripe.process', event, {
      uniqueKey: event.id, // dedupe replays
    })
    return { ok: true }
  }
}
```

## When you don't have a specific queue in mind

For code that targets queues dynamically (admin tools, generic dispatchers), inject the raw client with `@InjectEddyq()`:

```ts
import { InjectEddyq, type Eddyq } from '@eddyq/nestjs'

@Injectable()
export class Dispatcher {
  constructor(@InjectEddyq() private readonly eddyq: Eddyq) {}

  async send(queueName: string, kind: string, payload: unknown) {
    return this.eddyq.enqueue(kind, payload, { queue: queueName })
  }
}
```

Prefer `@InjectQueue(name)` for normal feature code — it carries defaults and keeps the queue name out of every call site.
