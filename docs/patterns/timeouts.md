# Job timeouts

**Problem.** A job legitimately takes a few seconds. But sometimes a downstream hangs, a connection wedges, or a runaway loop never returns. Without a bound, one stuck job ties up a worker slot until the lease expires.

**Solution.** Set a default per-job timeout on the queue. After the timeout, the job's `AbortSignal` fires; if your handler respects it, the job unwinds and retries.

```ts
// At most 60 seconds per job on the 'batch' queue
await q.setQueueTimeout('batch', 60_000)
```

Any `send.email` (or whatever) job enqueued onto `batch` is bound by 60s. To clear:

```ts
await q.setQueueTimeout('batch', null)
```

## Respecting the signal

A timeout only works if your handler observes `signal`. Pass it to `fetch`, `AbortController`, your DB driver's cancellation API, anything async:

```ts
q.work('long.task', async ({ signal, payload }) => {
  const res = await fetch(payload.url, { signal })
  await db.query('SELECT ...', { signal })
  return await processStream(res.body, { signal })
})
```

When the timeout fires, `signal.aborted` becomes true and the underlying `fetch` / `db.query` rejects with `AbortError`. Your handler's `try/catch` (or just letting it propagate) does the rest.

## What "timeout" actually does

eddyq doesn't terminate your handler. It fires `signal.abort()` and waits for the handler to return. If your handler ignores the signal and runs forever, the job will eventually be marked stale by the [sweeper](/node/maintenance#sweeper-recovering-stuck-jobs) — but that's the safety net, not the design.

**Always thread `signal` through.** It's the only way timeouts (and graceful shutdown) actually work.

## Per-kind vs per-queue

Timeouts are configured **per queue**, not per kind. If different kinds on the same queue need different bounds, route them onto different queues:

```ts
// API enqueue
await q.enqueue('quick.task', payload)                       // → 'default' queue, no timeout
await q.enqueue('long.report', payload, { queue: 'batch' })  // → 'batch' queue, 60s timeout
```

## Manually setting a shorter timeout for one job

There's no per-enqueue timeout option — that's deliberate. Configure timeouts as a property of the workload (the queue), not as a per-call thing. If you need finer control, run a `Promise.race` inside the handler:

```ts
q.work('flaky.api', async ({ signal, payload }) => {
  const result = await Promise.race([
    fetch(payload.url, { signal }),
    new Promise((_, reject) =>
      setTimeout(() => reject(new Error('hard timeout')), 5_000),
    ),
  ])
  // ...
})
```

Use sparingly — usually the queue-level timeout is the right knob.
