# Fail fast on Postgres outage

**Problem.** Your app boots, calls `Eddyq.connect`, and Postgres is unreachable. You want a **clear, fast error** — not a 30-second hang while sqlx retries connection in the background, and definitely not a worker that "starts" but actually polls forever against nothing.

**Solution.** Bound the connect timeout and let the failure propagate. Your orchestrator (k8s, systemd, your shell) does the rest.

```ts
import { Eddyq } from '@eddyq/queue'

async function main() {
  const q = await Eddyq.connect(process.env.DATABASE_URL!, {
    acquireTimeoutMs: 5_000,   // fail in 5s, not 30
  })

  q.work('send.email', handler)
  await q.start()
}

main().catch((err) => {
  console.error('boot failed:', err)
  process.exit(1)
})
```

If Postgres is down at boot, `connect` rejects within 5 seconds, the catch fires, and the process exits with status 1. Kubernetes (or systemd) restarts it; once Postgres is back, the next attempt succeeds.

## Don't catch-and-continue

A worker that boots without a database connection is worse than a worker that doesn't boot at all — it'll silently drop jobs you thought were running. **Let connection errors crash the process.**

```ts
// ❌ Don't do this
try {
  await Eddyq.connect(url)
} catch (err) {
  logger.warn('queue unavailable; running degraded')
  // ...continue without the queue
}
```

## At runtime

After `start()`, eddyq tolerates brief Postgres unavailability — pool reconnects, queries retry the next tick, in-flight jobs survive a 30-second blip if their lease is fresh.

For longer outages: the [sweeper](/node/maintenance#sweeper-recovering-stuck-jobs) will reclaim any jobs that were running when the connection dropped, once the database is back. You don't need to handle this in your code.

## Health checks

If you front your worker with a health endpoint (Kubernetes liveness probe, etc.), point it at `q.getStats()` or any cheap admin call:

```ts
@Get('/health')
async health() {
  await this.queue.getStats()  // throws if pool is broken
  return { ok: true }
}
```

If the call throws, the probe fails, kubelet restarts the pod. Same fail-fast principle, applied continuously.

## Migration check at boot

Separate from connection failures: `q.start()` refuses to boot if migrations are pending. This is also fail-fast — you find out at startup, not when a job tries to use a missing column.

```ts
await q.start()
// If migrations are pending, this throws with a message telling you exactly
// which version is missing and how to apply it.
```

See [Migrations](/guide/migrations).
