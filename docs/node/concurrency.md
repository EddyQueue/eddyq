# Parallelism & concurrency

eddyq has **four** layers of concurrency control. They stack — every job pull respects every layer above it. Understanding the model is the difference between "my queue scales" and "my queue thrashes."

| Layer | Scope | Set via |
|---|---|---|
| **Worker concurrency** | One process | `workerConcurrency` in `ConnectOptions` |
| **Queue concurrency** | All processes | `setQueueConcurrency(name, max)` |
| **Group concurrency** | All processes, per group key | `setGroupConcurrency(key, max)` |
| **Group rate limit** | All processes, per group key | `setGroupRate(key, count, periodMs)` |

The effective in-flight count is the **minimum** of every applicable layer.

## Layer 1 — worker concurrency

How many jobs **one process** runs at once. Default is `10`.

```ts
const q = await Eddyq.connect(url, { workerConcurrency: 50 })
```

This is your fleet's primary scaling knob. Each in-flight job consumes:

- A bit of Node memory (whatever your handler closes over)
- Outbound resources your handler uses (HTTP sockets, DB connections to other systems)
- **Briefly**, an eddyq pool connection (only during fetch / heartbeat / complete — not while the handler runs)

Pick `workerConcurrency` based on the cheapest of those three for your workload. CPU-bound work? Probably 1–4 per process. Network-bound, mostly waiting on `fetch`? 50–200 is fine.

**You can scale by raising `workerConcurrency` *or* by adding pods.** Both work. Pods cost more but bound the blast radius of one bad handler. Concurrency costs less but a memory leak in one handler taints all the others in that process.

## Layer 2 — queue concurrency

A fleet-wide cap on how many jobs from a [named queue](./queues) can be running at once, regardless of pod count.

```ts
await q.setQueueConcurrency('batch', 32)
```

Use when you want to bound a workload that you've split onto its own queue. Common case: a batch queue that you want capped to 32 in-flight regardless of how many worker pods you happen to have.

## Layer 3 — group concurrency

A fleet-wide cap on how many jobs sharing a `groupKey` can be running at once. The big differentiator vs BullMQ.

```ts
await q.setGroupConcurrency('tenant:42', 4)
```

Per-tenant fairness, per-provider rate budgeting, per-user free-tier limits. See the [groups page](./groups) for the full toolkit.

## Layer 4 — group rate limit

Token bucket — at most `count` jobs may *start* per `periodMs` window in a group:

```ts
await q.setGroupRate('provider:sendgrid', 100, 60_000)  // 100/minute
```

Different from concurrency caps: a rate limit doesn't care how *many* are in flight, only how *fast* they start.

## How they stack

A worker pulls a job only if **all** of these are true:

1. Its process is below `workerConcurrency`
2. The job's queue is below `queueConcurrency` (if set)
3. The job's group is below `groupConcurrency` (if set)
4. The job's group has a token available in its rate bucket (if set)

Otherwise the job stays `pending` and another worker (or the same worker, on its next tick) tries again.

## Picking numbers

A starting point for a typical web workload:

```ts
await Eddyq.connect(url, {
  maxConnections: 10,        // pool size, sized for the fleet
  workerConcurrency: 50,     // network-bound, lots of waiting on fetch
})

await q.setQueueConcurrency('default', 200)   // soft fleet-wide ceiling
await q.setGroupConcurrency('tenant:*', 8)    // multi-tenant fairness
```

The right numbers come from watching `runningCount` on `listGroups()` / `listNamedQueues()` and your downstream's metrics. There's no "correct" answer — these are operational dials, not architectural commitments. Change them at runtime; no deploy needed.
