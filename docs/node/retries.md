# Retries & cancellation

Job handlers signal their fate by returning, throwing, or throwing one of two typed errors.

## Default behavior

- **Returns a value** → success. Stored in `eddyq_jobs.result`.
- **Throws any error** → retried with exponential backoff up to `maxAttempts` (default 5).

## `RetryError` — explicit retry with custom delay

Use when you have first-party knowledge of *when* to retry — rate limits, scheduled maintenance windows, dependency outages.

```ts
if (res.status === 429) {
  const after = Number(res.headers.get('retry-after') ?? 60) * 1000
  throw new RetryError('rate limited', { delayMs: after })
}
```

The thrown error skips the backoff curve and uses your `delayMs` instead.

## `CancelError` — give up immediately

Use when the work can't succeed no matter how many times you try — invalid input, permanent 5xx from a downstream service, the user's account was deleted.

```ts
if (!res.ok && attempt >= 3) {
  throw new CancelError('permanent 5xx; giving up')
}
```

The job is marked `cancelled` and won't be retried.

## Inspecting attempts

`attempt` in the handler context tells you which try this is (1-indexed). Combine with `maxAttempts` to add late-stage logic:

```ts
q.work('flaky', async ({ attempt, maxAttempts }) => {
  try {
    return await doThing()
  } catch (err) {
    if (attempt === maxAttempts) {
      await reportToSentry(err)
    }
    throw err
  }
})
```
