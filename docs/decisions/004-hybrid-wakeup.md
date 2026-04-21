# 004 — Job wakeup: hybrid poll + LISTEN/NOTIFY
Status: Accepted
Date: 2026-04-21

## Context

A job queue needs to know when new work arrives. The two primitives Postgres gives us are:

- **Polling** — workers periodically `SELECT ... FOR UPDATE SKIP LOCKED`. Simple, reliable, PgBouncer-compatible in all pool modes, but introduces enqueue→pickup latency bounded by the poll interval.
- **LISTEN/NOTIFY** — producer calls `pg_notify`, consumer receives near-instantly over a long-lived connection. Low latency, but:
  - "At most once" delivery; no guarantee the signal is received.
  - Postgres acquires a global commit-time lock during NOTIFY, serializing all commits. Under concurrent writes this is a measurable bottleneck. ([Recall.ai postmortem](https://www.recall.ai/blog/postgres-listen-notify-does-not-scale))
  - Transaction-pooled PgBouncer breaks LISTEN (the notification may arrive on a connection the client no longer holds).
  - Naive "NOTIFY per INSERT" floods both the commit path and the listener during bursts.

River (Go, riverqueue.com) has operated a production hybrid of both primitives for multiple years. Their design has converged on four specific lessons that are easy to under-appreciate from a distance.

## Decision

**Run both primitives simultaneously.** LISTEN/NOTIFY is the fast path. Polling is the safety net.

Expose three configuration knobs:

- **`fetch_poll_interval`** (default 1s) — how often the fallback poll fires. Catches missed NOTIFY signals, fetch-limiter skips, and runs unconditionally in `poll_only` mode.
- **`fetch_cooldown`** (default 100ms) — minimum gap between fetches regardless of wakeup trigger. A NOTIFY burst (say 500 inserts arriving simultaneously) does not trigger 500 back-to-back fetches; it triggers one, then another once the cooldown elapses.
- **`poll_only`** (default `false`) — explicit opt-out of LISTEN/NOTIFY for transaction-pooled-PgBouncer deployments. When `true`, enqueue→pickup latency is bounded by `fetch_poll_interval`.

**Apply 10% jitter** to `fetch_poll_interval`: each poll cycle sleeps `base + uniform[0, base/10)` with a minimum jitter of 10ms. This desynchronizes multi-worker fleets on boot — without jitter, 50 workers started together will all poll on the same tick and thundering-herd the fetch query.

**Debounce NOTIFY on the producer side.** The enqueue path keeps a per-channel "last notified at" timestamp in memory; `pg_notify` is called at most once per debounce window (default 100ms). Consumers only need to know "stuff exists" — not how much.

## Alternatives considered

- **Polling-only** — Simpler, no PgBouncer gotchas, but the enqueue→pickup latency floor at ~500ms to 1s (depending on `fetch_poll_interval`) is unacceptable for real-time-feeling workloads. Also: you lose one of Postgres's built-in scaling tools for no good reason.
- **LISTEN/NOTIFY-only** — Breaks behind transaction-pooled PgBouncer, which is the default deployment for most high-concurrency Postgres apps. Also "at most once" — missed signals mean stuck jobs.
- **Advisory locks + sleep** — Unproven, and requires the same kind of fallback polling anyway.
- **External message bus (Redis, NATS) for notifications** — Defeats the "Postgres only" positioning. This is for v2.x if ever.

## Consequences

- The default wakeup latency is bounded by `fetch_cooldown` (typically a few hundred ms) when NOTIFY works, and `fetch_poll_interval` when it doesn't. Both are user-tunable.
- Producers debounce NOTIFY in-memory per connection. A multi-process producer fleet still over-notifies proportionally to process count; documented as a tuning consideration.
- Workers require a dedicated LISTEN connection (not pooled) to subscribe. This couples to ADR 010's PgBouncer matrix: the worker coordinator connection bypasses PgBouncer (or uses session-mode pooling), while enqueue-path connections can use transaction-mode pooling freely.
- `poll_only` is the escape hatch for users who can't grant the coordinator a session-mode connection. It trades latency for operational simplicity.
- Benchmark harness (Phase 0) measures the cost of the NOTIFY path at 10k+ inserts/sec to confirm debouncing holds up and the commit-time lock is not a bottleneck at target scale.
