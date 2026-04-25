# Idempotent jobs

**Problem.** An upstream source delivers events at-least-once: webhooks, retried HTTP calls, "click submit twice" buttons, replayed Kafka messages. You want exactly-once *processing* — the second delivery should be a no-op.

**Solution.** Set `uniqueKey` to a stable identifier of the event. Duplicate enqueues with the same key are silently skipped while a job is active.

```ts
@Post('webhooks/stripe')
async handle(@Body() event: StripeEvent) {
  await q.enqueue('stripe.process', event, {
    uniqueKey: event.id,           // Stripe gives you a stable id
  })
  return { ok: true }
}
```

## Picking a key

The key is whatever uniquely identifies the *unit of work*. Some patterns:

| Source | Key shape |
|---|---|
| Webhook | `uniqueKey: event.id` |
| Once-per-user-event | `uniqueKey: \`welcome:${user.id}\`` |
| Once-per-day | `uniqueKey: \`digest:${user.id}:${ymd}\`` |
| Hash of a payload | `uniqueKey: \`upload:${file.sha256}\`` |
| Compound | `uniqueKey: \`stripe:${customer.id}:${invoice.id}\`` |

## Scope (important)

The dedupe constraint is `UNIQUE (kind, unique_key) WHERE state IN ('pending', 'scheduled', 'running')`.

- **Per-kind.** `uniqueKey: 'event-123'` for `send.email` does **not** conflict with the same key for `process.payment`. Namespace your key (`'webhook:event-123'`) if you want cross-kind dedupe.
- **Active jobs only.** Once a job reaches `completed`/`failed`/`cancelled`, the key is freed. A new enqueue with the same key inserts a fresh row.

This is "don't enqueue twice while one is in the pipeline," **not** "this event has happened before, ever." For historical dedupe, store keys in your own table.

## Detecting a duplicate

```ts
const r = await q.enqueue('stripe.process', event, { uniqueKey: event.id })
if (!r.inserted) {
  // Duplicate — original job already exists. Usually fine to ignore.
}
```

`enqueueMany` returns aggregate counts: `{ inserted, skipped }`.

## With transactional enqueue

`uniqueKey` and `tx` compose. Inside a transaction, the dedupe check uses the transaction's snapshot — so two parallel transactions trying to insert the same key will serialize, with one inserting and the other skipping.

```ts
await pg.tx(async (tx) => {
  await tx.insertInvoice({ id })
  await q.enqueue('invoice.email', { id }, { uniqueKey: `inv:${id}`, tx })
})
```
