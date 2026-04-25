# Node.js quick start

A worker that processes `send.email` jobs, retries on rate limits, and shuts down gracefully.

```ts
import { Eddyq, CancelError, RetryError } from '@eddyq/queue'

const q = await Eddyq.connect(process.env.DATABASE_URL!)

q.work('send.email', async ({ payload, id, attempt, signal }) => {
  const res = await fetch(payload.url, { signal })

  if (res.status === 429) {
    const after = Number(res.headers.get('retry-after') ?? 60) * 1000
    throw new RetryError('rate limited', { delayMs: after })
  }
  if (!res.ok && attempt >= 3) {
    throw new CancelError('permanent 5xx; giving up')
  }

  return { bytes: (await res.text()).length }
})

await q.start()

// ... your app runs ...

await q.shutdown(10_000) // 10s grace; fires abort signals on inflight jobs
```

## Connecting

`Eddyq.connect` takes a Postgres connection string — same shape as `DATABASE_URL`, `pg`, or `psql`. It does **not** accept individual host/port/user/password fields, so if your config is structured (Vault, k8s secrets, a config object), compose the URL on the way in.

::: code-group

```ts [From DATABASE_URL]
const q = await Eddyq.connect(process.env.DATABASE_URL!)
```

```ts [From individual parts]
const { user, pass, host, port, db } = config.postgres
const url = `postgres://${encodeURIComponent(user)}:${encodeURIComponent(pass)}@${host}:${port}/${db}`
const q = await Eddyq.connect(url)
```

```ts [With pool tuning]
const q = await Eddyq.connect(process.env.DATABASE_URL!, {
  maxConnections: 10,   // sized for your fleet, not just one process
  pollOnly: true,       // required for transaction-mode PgBouncer
})
```

:::

See [`ConnectOptions`](/api/queue/interfaces/ConnectOptions) for the full options reference, including the LISTEN/NOTIFY tradeoffs.

## Enqueue from anywhere

```ts
await q.enqueue('send.email', { url: 'https://example.com/welcome' })
```

## Transactional enqueue

Enqueue inside the same transaction as your business write — the job only becomes visible if the row commits.

```ts
await pg.tx(async (tx) => {
  const user = await tx.insertUser({ email: 'a@b.com' })
  await q.enqueue('send.email', { userId: user.id }, { tx })
})
```

## Next steps

- [Workers](./workers) — concurrency, group limits, lifecycle
- [Retries & cancellation](./retries) — `RetryError`, `CancelError`, attempt counts
- [Schedules](./schedules) — declarative cron schedules
- [API: `@eddyq/queue`](/api/queue/) — full reference
