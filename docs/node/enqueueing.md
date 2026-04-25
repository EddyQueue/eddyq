# Enqueueing jobs

```ts
await q.enqueue('send.email', { url: 'https://example.com' })
```

Returns the job ID immediately once the row is committed.

## Options

```ts
await q.enqueue('send.email', payload, {
  delayMs: 60_000,           // run no earlier than +60s
  priority: 10,              // higher = sooner; default 0
  uniqueKey: 'user:42:welcome', // dedupe — second enqueue is a no-op
  maxAttempts: 5,            // override the worker's default
  tx,                        // see "Transactional enqueue" below
})
```

## Idempotent enqueue with `uniqueKey`

`uniqueKey` is **optional**. Set it when you want the second enqueue of the "same" job to be a silent no-op rather than a duplicate row. This is the standard fix for at-least-once delivery from upstream sources — webhooks, retries, replays, "click submit twice" buttons.

```ts
await q.enqueue('send.email', payload, {
  uniqueKey: `welcome:${user.id}`,
})
```

What "duplicate" means is whatever your key encodes. Common patterns:

| Use case | Key shape |
|---|---|
| Webhook idempotency | `uniqueKey: event.id` |
| Once-per-user-event | `uniqueKey: \`welcome:${user.id}\`` |
| Once-per-day | `uniqueKey: \`digest:${user.id}:${ymd}\`` |
| Bursty client retries | `uniqueKey: \`upload:${file.sha256}\`` |

A duplicate enqueue returns the existing job's outcome — your call site can't tell whether it inserted or skipped without checking `outcome.deduped`. That's usually what you want.

**Without `uniqueKey`, every `enqueue` call inserts a new row.** That's also fine — you just get a new job each time. Use the key only when dedupe is the behavior you actually want.

### Scope

The dedupe constraint is `UNIQUE (kind, unique_key) WHERE state IN ('pending', 'scheduled', 'running')`. Two implications:

- **Per-kind, not global.** `uniqueKey: 'event-123'` for `send.email` does **not** conflict with `uniqueKey: 'event-123'` for `process.payment`. If you want cross-kind dedupe, namespace your key (`'webhook:event-123'`).
- **Active jobs only.** Once a job reaches `completed`, `cancelled`, or `failed`, its key is freed and a new enqueue inserts a fresh row. `uniqueKey` is "don't enqueue twice while one is already in the pipeline," **not** "this job has happened before, ever."

If you need true historical dedupe ("I've ever seen this event"), store the key in your own table.

## Transactional enqueue

Pass an existing `pg` (or any compatible) transaction client as `tx`. The job becomes visible only if the surrounding transaction commits.

```ts
await pg.tx(async (tx) => {
  const user = await tx.insertUser({ email: 'a@b.com' })
  await q.enqueue('send.email', { userId: user.id }, { tx })
})
```

This eliminates the classic "the job ran before the row committed" bug.

## Bulk enqueue

```ts
await q.enqueueMany([
  { name: 'send.email', payload: { id: 1 } },
  { name: 'send.email', payload: { id: 2 } },
  { name: 'rebuild.thumb', payload: { id: 7 }, delayMs: 5_000 },
])
```

Single round trip. Returns an array of `{ id, deduped }` outcomes in input order.
