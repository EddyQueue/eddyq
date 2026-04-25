# Graceful shutdown

```ts
await q.shutdown(10_000) // wait up to 10s for in-flight jobs
```

`shutdown(graceMs)`:

1. Stops polling for new jobs immediately.
2. Fires `signal.abort()` on every in-flight job's `AbortSignal`.
3. Waits up to `graceMs` for handlers to return.
4. Closes the database pool.

## Wire it to SIGTERM

```ts
const q = await Eddyq.connect(process.env.DATABASE_URL!)
// ... q.work(...), await q.start() ...

for (const sig of ['SIGINT', 'SIGTERM']) {
  process.once(sig, async () => {
    await q.shutdown(10_000)
    process.exit(0)
  })
}
```

## Inside a handler

Always pass `signal` through to anything that supports it — `fetch`, `AbortController`, your DB driver's cancellation API.

```ts
q.work('long.task', async ({ signal }) => {
  const res = await fetch(url, { signal })
  // signal will throw on shutdown, your handler unwinds, eddyq retries the job
})
```

## What happens if a job exceeds the grace period?

The connection closes mid-execution. The job remains `running` until its lease expires (default 5 minutes), then becomes available for retry.
