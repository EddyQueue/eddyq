# Bulk enqueue across queues

**Problem.** You're processing a webhook that needs to fan out to multiple jobs — say, an email, a thumbnail rebuild, and an audit log entry — possibly on different queues. You don't want N round trips.

**Solution.** `enqueueMany` takes a heterogeneous batch. Each item carries its own `kind`, `queue`, `groupKey`, `delayMs`, etc.

```ts
await q.enqueueMany([
  { kind: 'send.email',     payload: { to: user.email },        queue: 'default' },
  { kind: 'rebuild.thumb',  payload: { id: photo.id },          queue: 'batch' },
  { kind: 'audit.log',      payload: { event: 'user.created' }, queue: 'low-priority' },
])
```

One Postgres `INSERT` regardless of batch size. Mixed `kind`, mixed `queue`, mixed `groupKey` — all fine in the same call.

## Returns aggregate counts

```ts
const r = await q.enqueueMany(items)
// { inserted: 3, skipped: 0 }
```

Per-job IDs are **not** returned. If you need to correlate results back to your domain objects, attach a stable `uniqueKey`:

```ts
await q.enqueueMany(events.map(e => ({
  kind: 'webhook.process',
  payload: e,
  uniqueKey: `evt:${e.id}`,
})))
```

## Limits

- **5,000 items per call** — split larger batches client-side.
- **All-or-nothing within a batch.** A SQL error rolls back the whole insert; partial failures aren't possible.

## When to prefer single `enqueue`

- You need the auto-generated job ID immediately (e.g. to return it from an HTTP response).
- The fan-out is conditional on the result of an earlier insert in the same transaction.

For everything else, `enqueueMany` is the default.
