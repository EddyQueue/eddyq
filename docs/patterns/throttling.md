# Throttling jobs

**Problem.** A downstream API has a rate budget — Sendgrid 100/day on the free tier, OpenAI 3 RPM on a low tier, your S3 bucket's `PutObject` throttle. Without bounds, a backlog drain blows your quota and tail jobs fail with 429s.

**Solution.** Tag jobs with a `groupKey` for the downstream, then set a token-bucket rate limit on the group.

```ts
// Tag at enqueue time
await q.enqueue('send.email', payload, {
  groupKey: 'provider:sendgrid',
})

// Bound the group rate. At most 100 jobs may *start* per minute.
await q.setGroupRate('provider:sendgrid', 100, 60_000)
```

Workers pulling jobs check the bucket. When it's empty, the job stays `pending` until a token is available. No client-side sleeps, no timer drift.

## Stacking with concurrency

Rate limits and concurrency caps can both apply to the same group. eddyq respects whichever is more restrictive at any moment.

```ts
await q.setGroupConcurrency('provider:sendgrid', 4)        // never more than 4 in flight
await q.setGroupRate('provider:sendgrid', 100, 60_000)     // and never more than 100/min start
```

Use both when the downstream has *both* a concurrency cap (e.g. "max 4 connections") *and* a rate cap (e.g. "100 RPM").

## Picking a window

The window is the *minimum* duration over which the rate is averaged. A larger window allows more burstiness, a smaller one is stricter.

| Use case | Suggestion |
|---|---|
| Strict per-second rate (financial APIs) | `count=N, periodMs=1_000` |
| Daily quota (Sendgrid free) | `count=100, periodMs=86_400_000` |
| Smooth steady throughput | `count=N, periodMs=60_000` |

## Different downstreams = different groups

Don't put unrelated work in the same group just to share a limit:

```ts
// ❌ "all third-party calls" — too broad
await q.enqueue('send.email',  p, { groupKey: 'external' })
await q.enqueue('post.slack',  p, { groupKey: 'external' })
await q.enqueue('upload.s3',   p, { groupKey: 'external' })

// ✅ Each downstream gets its own group + budget
await q.enqueue('send.email',  p, { groupKey: 'sendgrid' })
await q.enqueue('post.slack',  p, { groupKey: 'slack' })
await q.enqueue('upload.s3',   p, { groupKey: 's3:my-bucket' })
```

## Per-tenant variants

The group key is just a string — encode whatever dimensions matter:

```ts
// Per-tenant Sendgrid budget (each tenant gets their own quota)
groupKey: `sendgrid:${tenantId}`

// Per-API-key OpenAI budget (one group per tenant's BYO key)
groupKey: `openai:${tenantId}:${keyId}`
```

## Removing a limit

```ts
await q.clearGroupRate('provider:sendgrid')
```

Concurrency cap stays in place — only the rate bucket is cleared.
