# Deployment

eddyq is a long-running stateful service. There's no special infrastructure — just Node processes that hold a Postgres pool. But there are a few production-shaped knobs worth understanding before you ship.

## Connection pool sizing

Each eddyq process opens `maxConnections + 1` Postgres connections (the `+1` is a dedicated `LISTEN` socket). Multiply by your replica count.

```
total_pg_connections = replica_count × (maxConnections + 1)
```

Defaults: `maxConnections = 5`. At 10 replicas → 60 connections. Postgres ships with `max_connections = 100` — so an unconfigured fleet can lock you out fast.

Sizing rule of thumb:

- Job handlers do **not** hold a connection while running. The pool is touched briefly for fetch, heartbeat, and complete/fail.
- `maxConnections ≈ workerConcurrency / 5` is a fine starting point.
- Behind PgBouncer, set `maxConnections` to your **per-process PgBouncer pool allocation**, not raw Postgres `max_connections`.

```ts
await Eddyq.connect(process.env.DATABASE_URL!, {
  maxConnections: 10,
  minConnections: 2,
})
```

## PgBouncer

eddyq uses Postgres `LISTEN/NOTIFY` to wake workers immediately when a new job is enqueued. This requires a **persistent session connection**.

| PgBouncer mode | Works with `LISTEN`? | What to do |
|---|---|---|
| `session` | ✅ | No changes needed |
| `transaction` | ❌ | Set `pollOnly: true` |
| `statement` | ❌ | Set `pollOnly: true` (also: don't use this for any app) |

```ts
await Eddyq.connect(process.env.DATABASE_URL!, {
  pollOnly: true,
})
```

In poll-only mode, workers fall back to a 1-second poll interval. Idle latency is bounded by that — under load the poll runs faster anyway.

## Splitting API and worker processes

A common production layout: API pods enqueue jobs and serve admin reads, worker pods actually process jobs. They're the same code, different startup.

```ts
// api.ts — does NOT call start(), only enqueues
const q = await Eddyq.connect(url)
// ... wire q.enqueue into your HTTP handlers ...

// worker.ts — calls start(), processes jobs
const q = await Eddyq.connect(url)
q.work('send.email', handler)
q.work('process.payment', handler)
await q.start()
```

Why split:

- **Different scaling.** API scales with HTTP traffic; workers scale with queue depth.
- **Blast radius.** A poisonous job that ramps memory shouldn't kill your API.
- **Restarts.** Worker rollouts can take their time; API rollouts shouldn't.

NestJS users get this split for free — see the [example app](/nestjs/example).

## Replica count and leader election

Any number of workers can run. One is automatically elected leader to run [maintenance loops](./maintenance) — no separate scheduler pod required. Failover takes ~30s if the leader dies.

For safety: run **at least 2 worker replicas** in production. With one replica, all maintenance pauses while you redeploy.

## Multi-region & primary failover

eddyq talks to one Postgres at a time. If you fail over to a replica:

1. Stop your eddyq pods (or let them error and restart).
2. Promote the replica to primary.
3. Start eddyq pointed at the new primary.

There's no built-in multi-primary support — Postgres doesn't really do that, and a job queue across primaries would need a coordinator we deliberately don't have.

## Migrations

Always apply migrations as a deploy step **before** booting workers. `q.start()` refuses to boot against a stale schema. See [Migrations](/guide/migrations) for the full rationale and recipes.
