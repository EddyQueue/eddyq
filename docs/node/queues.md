# Named queues

Every job lives on a **queue** — a top-level routing label. By default, jobs go to the `default` queue and every worker subscribes to `default`. For most apps, that's all you need.

Named queues become useful when you want:

- **Different worker pools for different workloads.** A small pool for low-latency jobs, a large pool for batch processing.
- **Per-queue concurrency limits.** Cap the total in-flight count for an entire workload, regardless of group keys.
- **Per-queue default timeouts.** Force long-running batch jobs to bound their runtime.
- **Per-queue pause.** Drain one queue without stopping everything else.

## Routing a job

Pass `queue` at enqueue time:

```ts
await q.enqueue('send.email', payload, { queue: 'low-priority' })
```

Without `queue`, jobs land on `default`.

## Subscribing workers to specific queues

By default, a worker processes the `default` queue only. To pull from other queues, subscribe explicitly. The fetcher filters at the SQL level (`queue = ANY($queues)`), so an unsubscribed queue is invisible to that worker — no claim, no skip, no waste.

::: code-group

```ts [@eddyq/queue]
const q = await Eddyq.connect(process.env.DATABASE_URL!)
q.subscribeTo(['emails', 'webhooks'])  // ignores 'default', 'batch', anything else
q.work('send.email', handler)
await q.start()
```

```ts [@eddyq/nestjs]
EddyqModule.forRoot({
  databaseUrl: process.env.DATABASE_URL!,
  subscribeTo: ['emails', 'webhooks'],   // this worker pool only pulls these
})
```

:::

Call `subscribeTo` **before** `start()` (the NestJS module wires this for you on bootstrap). Default if omitted is `['default']`.

Splitting your fleet by queue is the standard way to give different workloads dedicated capacity:

```
api pods:    no q.start() — only enqueue
web pods:    subscribeTo(['default'])           — fast, latency-sensitive
batch pods:  subscribeTo(['batch', 'reports'])  — large memory, long running
```

## Queue-level controls

Same admin shape as groups, but scoped to the queue:

```ts
await q.setQueueConcurrency('batch', 32)   // at most 32 in-flight, fleet-wide
await q.setQueueTimeout('batch', 60_000)   // 60s default per-job timeout
await q.pauseQueue('batch')                 // stop pulling, in-flight finish
await q.resumeQueue('batch')
```

`setQueueTimeout(queue, null)` clears the default.

## Inspecting queues

```ts
const queues = await q.listNamedQueues()
// NamedQueue { name, runningCount, maxConcurrency, paused, defaultTimeoutMs, ... }
```

Returns queues that have explicit admin rows (cap, pause, timeout). To see live job counts across all queues — including those without explicit rows — use `getStats()`.

## When to split

Don't split queues until you have a reason. Each queue is one more thing to monitor. Good reasons:

- A workload genuinely needs different scaling characteristics.
- A workload's failure mode would otherwise contaminate everything (e.g. a flaky third-party API).
- An on-call team owns one workload but not others.

Bad reasons:

- "Organizational tidiness." Use job kinds (`send.email` vs `process.payment`) for that — they're free.
- "We might want to scale them differently someday." Wait until the day comes.
