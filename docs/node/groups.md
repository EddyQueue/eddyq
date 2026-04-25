# Group concurrency & rate limits

A **group** is a free-form string label you attach to jobs at enqueue time. eddyq lets you cap concurrency and rate-limit per group across your entire fleet — not per process, not per worker, but globally.

This is the headline feature for multi-tenant systems: one rude tenant can't starve the rest of your customers, no matter how many workers you scale to.

## Tagging a job

Pass `groupKey` when enqueueing:

```ts
await q.enqueue('send.email', payload, {
  groupKey: `tenant:${tenantId}`,
})
```

Jobs without a `groupKey` are unconstrained.

## Capping concurrency

```ts
// At most 4 concurrent jobs in tenant:42, fleet-wide.
await q.setGroupConcurrency('tenant:42', 4)
```

Workers pulling jobs check the cap before claiming. If the group is at its cap, the job stays in `pending` until another job in the group finishes. Other groups are unaffected.

Common patterns:

| Group key | Use case |
|---|---|
| `tenant:${id}` | Fairness across customers |
| `provider:sendgrid` | Don't blow your downstream's quota |
| `user:${id}` | Per-user fairness in a free tier |
| `webhook:stripe` | Serialize side effects per source |

## Rate limiting

Token-bucket rate limit — at most `count` jobs may *start* per `periodMs` window:

```ts
// Sendgrid Free tier: 100/day = ~4/hour
await q.setGroupRate('provider:sendgrid', 4, 60 * 60 * 1000)
```

Concurrency caps and rate limits stack. A group can have both — eddyq respects whichever is more restrictive at any moment.

```ts
await q.clearGroupRate('provider:sendgrid')
```

## Pause / resume

Stop a group dead:

```ts
await q.pauseGroup('tenant:42')   // no new jobs start; in-flight finish
await q.resumeGroup('tenant:42')
```

Useful for incident response — pause a noisy tenant while you investigate, no deploy needed.

## Inspecting groups

```ts
const groups = await q.listGroups()
// Group { key, runningCount, maxConcurrency, paused, ratePerPeriod, periodMs }
```

Groups appear here once you've called any of the setters above. A group with no explicit row is implicitly unconstrained.

## State lives in Postgres

All group state — caps, rate limits, pause flags — is durable. Restart your fleet and limits stay in effect. There's no in-memory token bucket to lose.
