# Stop retrying jobs

**Problem.** A job has failed in a way that won't be fixed by retrying — bad input, deleted upstream resource, permanent 4xx. The default retry behavior wastes attempts (and your dashboards' signal-to-noise).

**Solution.** Throw `CancelError` from the handler. The job is marked `cancelled` immediately and won't be retried.

```ts
import { CancelError } from '@eddyq/queue'

q.work('send.email', async ({ payload }) => {
  const user = await db.users.findById(payload.userId)
  if (!user) {
    throw new CancelError('user no longer exists; nothing to email')
  }
  if (user.bouncedAt && user.bouncedAt > Date.now() - 90 * 86400_000) {
    throw new CancelError('email address bounced recently; suppressed')
  }
  await sendgrid.send({ to: user.email, ...payload })
})
```

`cancelled` is a terminal state — distinct from `failed` so your dashboards can separate "this kept retrying and gave up" from "we deliberately gave up early."

## When to use

| Situation | Throw |
|---|---|
| Bad input that won't get better | `CancelError` |
| Resource was deleted upstream | `CancelError` |
| Permanent 4xx from a downstream | `CancelError` |
| Transient 5xx | (regular error — retries) |
| Rate limit (429) | `RetryError({ delayMs })` |
| Race condition with a separate write | (regular error — retries) |

If you're not sure, throw a regular error. Eating attempts on the rare false positive is better than dropping a job that *would* have succeeded.

## Inspecting how many times we tried

Combine `attempt` and `maxAttempts` from the handler context to escalate before cancelling:

```ts
q.work('flaky', async ({ attempt, maxAttempts, payload }) => {
  try {
    return await doThing(payload)
  } catch (err) {
    if (attempt === maxAttempts) {
      await reportToSentry(err, { jobPayload: payload })
    }
    throw err   // let eddyq run the normal retry/fail flow
  }
})
```

## Cancelling from outside the handler

If you find a stuck or no-longer-relevant job from admin or a UI:

```ts
const cancelled = await q.cancel(jobId)
// true if cancelled; false if the job doesn't exist or already finalized
```

`cancel()` only works on `pending`, `scheduled`, or `running` jobs. Running jobs receive `signal.abort()` — your handler must respect the signal for cancellation to take effect mid-job (otherwise it finishes normally and the cancel is recorded after-the-fact).
