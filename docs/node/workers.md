# Workers

A **worker** is the function you register with `q.work(name, handler)`. eddyq invokes it for each job published under that name.

```ts
q.work('send.email', async ({ payload, id, attempt, signal }) => {
  await sendEmail(payload, { signal })
})
```

The handler receives a context object:

| Field | Type | Description |
|---|---|---|
| `payload` | `T` | The JSON value passed to `enqueue` |
| `id` | `string` | Unique job ID (UUID) |
| `attempt` | `number` | Current attempt, starting at 1 |
| `signal` | `AbortSignal` | Aborts on shutdown or job cancellation |

## Concurrency

Workers run jobs concurrently up to a per-name limit. Default is `1`.

```ts
q.work('send.email', handler, { concurrency: 16 })
```

## Group concurrency

Limit concurrent jobs per logical group — per tenant, per provider, per anything. Useful for fairness and rate-limit avoidance.

```ts
q.work('send.email', handler, {
  concurrency: 100,
  group: (job) => job.payload.tenantId,
  groupConcurrency: 4, // each tenant gets at most 4 in-flight
})
```

## Lifecycle

`q.work(...)` registers the handler but doesn't start polling. Workers begin pulling jobs after `q.start()`.

```ts
q.work('a', handlerA)
q.work('b', handlerB)
await q.start() // both begin polling
```

See [Graceful shutdown](./shutdown) for the other end.
