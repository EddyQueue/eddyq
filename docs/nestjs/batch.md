# Batches

Use `enqueueBatch` when you need a fan-in callback — N items of work that should fire one notification when every item reaches a terminal state. Eddyq tracks the count atomically in Postgres and fires the callback exactly once.

```ts
import { Body, Controller, Post } from '@nestjs/common'
import { InjectEddyq, type Eddyq } from '@eddyq/nestjs'

@Controller('reports')
export class ReportsController {
  constructor(@InjectEddyq() private readonly queue: Eddyq) {}

  @Post('run-shards')
  async runShards(@Body() body: { scope: string; shards: number }) {
    const stamp = Date.now()
    return this.queue.enqueueBatch({
      items: Array.from({ length: body.shards }, (_, i) => ({
        kind: 'report.shard',
        payload: { scope: body.scope, shard: i },
        uniqueKey: `report:${body.scope}:${stamp}:${i}`,
      })),
      onComplete: {
        kind: 'report.summary',
        payload: { scope: body.scope, runAt: stamp },
      },
      metadata: { scope: body.scope, runAt: stamp },
    })
  }
}
```

`enqueueBatch` returns `{ batchId, inserted, skipped }`. `skipped` are items that conflicted on `uniqueKey` — they don't count toward the batch's total, so a fully-deduped resubmit is a no-op (callback already fired on the first run).

## Handling the callback

The callback is just another job. Its kind is whatever you set on `onComplete.kind`. Eddyq merges a `_eddyq_batch` envelope into the payload:

```ts
import { Logger } from '@nestjs/common'
import { JobHandler, Processor, type JobCall } from '@eddyq/nestjs'

@Processor()
export class ReportsProcessor {
  private readonly logger = new Logger(ReportsProcessor.name)

  @JobHandler('report.shard')
  async shard({ payload }: JobCall) {
    // ... process this slice
  }

  @JobHandler('report.summary')
  async summary({ payload }: JobCall) {
    const { _eddyq_batch, scope } = payload as {
      _eddyq_batch: {
        batchId: number
        total: number
        completed: number
        failed: number
        cancelled: number
        durationMs: number
      }
      scope: string
    }
    if (_eddyq_batch.failed > 0) {
      this.logger.warn(
        `${scope}: ${_eddyq_batch.completed}/${_eddyq_batch.total} ok ` +
          `(${_eddyq_batch.failed} failed) in ${_eddyq_batch.durationMs}ms`,
      )
      return
    }
    this.logger.log(`${scope}: all ${_eddyq_batch.total} shards in ${_eddyq_batch.durationMs}ms`)
  }
}
```

The callback fires regardless of outcome mix — branch on `failed` / `cancelled` if you want to treat partial failures differently from a clean run. A "failed" item here means terminal failure (exhausted retries) or an explicit `CancelError`, not a transient throw that will retry.

## Without a callback

Drop `onComplete` and the batch row still tracks state for admin visibility — you can query the wakeboard or `eddyq_batches` directly to see progress without coupling to a callback handler.

```ts
await this.queue.enqueueBatch({
  items: [...],
  metadata: { source: 'admin-replay' },
})
```

## Caps

`items` shares the 5,000-per-call cap with `enqueueMany`. For larger batches, chunk on the client and use a deterministic `metadata.runId` to merge them in your own dashboard, or wait for a future `extendBatch` API.
