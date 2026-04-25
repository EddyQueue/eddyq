# Injecting the queue

Inject `EddyqQueue` anywhere you need to enqueue jobs.

```ts
import { Injectable } from '@nestjs/common'
import { InjectEddyq, EddyqQueue } from '@eddyq/nestjs'

@Injectable()
export class SignupService {
  constructor(@InjectEddyq() private readonly queue: EddyqQueue) {}

  async signup(email: string) {
    const user = await this.users.create({ email })
    await this.queue.enqueue('send.email', {
      to: email,
      template: 'welcome',
    })
    return user
  }
}
```

## Transactional enqueue

Pass a transaction client as `tx` — the job becomes visible only when the surrounding transaction commits.

```ts
@Injectable()
export class SignupService {
  constructor(
    @InjectEddyq() private readonly queue: EddyqQueue,
    private readonly db: DbService,
  ) {}

  async signup(email: string) {
    return this.db.tx(async (tx) => {
      const user = await tx.users.create({ email })
      await this.queue.enqueue('send.email', { userId: user.id }, { tx })
      return user
    })
  }
}
```

## From a controller

```ts
@Controller('webhooks')
export class WebhookController {
  constructor(@InjectEddyq() private readonly queue: EddyqQueue) {}

  @Post('stripe')
  async handle(@Body() event: StripeEvent) {
    await this.queue.enqueue('stripe.process', event, {
      uniqueKey: event.id, // dedupe replays
    })
    return { ok: true }
  }
}
```
