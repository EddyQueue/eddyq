# 010 — Three supported PgBouncer deployment modes
Status: Accepted
Date: 2026-04-21

## Context

PgBouncer is the most common Postgres connection pooler in production, and its pool modes interact non-obviously with the two wakeup primitives from ADR 004 (polling and LISTEN/NOTIFY) and with sqlx's use of prepared statements. Getting this wrong means users either hit mysterious failures or give up on eddyq entirely.

The three PgBouncer pool modes:

- **Session pooling** — A client holds a pooled connection for the duration of its session. LISTEN/NOTIFY works. Prepared statements work. Wastes connections under high concurrency.
- **Transaction pooling** — A client holds a pooled connection only for the duration of a transaction. LISTEN breaks (the notification may arrive on a connection the client no longer owns). Prepared statements require PgBouncer 1.21+. Maximal pooling efficiency.
- **Statement pooling** — A client gets a fresh connection per statement. Both LISTEN and prepared statements break. Not supported by sqlx (or by most modern ORMs).

eddyq has two distinct connection classes with different pooling needs:

- **Enqueue path** — high-volume, short-lived, transactional. Wants transaction pooling.
- **Worker coordinator** — low-volume, long-lived, uses LISTEN/NOTIFY. Requires session pooling or direct connection.

## Decision

Document three officially supported deployment modes. Default configuration (`poll_only = false`) works in modes 1 and 2; mode 3 requires `poll_only = true`.

### Mode 1 — No PgBouncer / session pooling everywhere

Everything uses the same pool. LISTEN/NOTIFY works. Lowest wakeup latency.

```
app → Postgres (direct)
  or
app → PgBouncer (session mode) → Postgres
```

Recommended for: small-to-medium deployments, local development, environments without PgBouncer.

### Mode 2 — Transaction pooling for enqueue, session/direct for coordinator (RECOMMENDED for PgBouncer users)

Enqueue traffic (95% of volume) goes through PgBouncer transaction-mode pool. Worker coordinator holds 2–5 direct (or session-pooled) connections for LISTEN/NOTIFY and maintenance.

```
app enqueue path  → PgBouncer (transaction mode) → Postgres
  eddyq worker    → direct                        → Postgres  (2-5 conns)
```

Requires PgBouncer 1.21+ for prepared statements in transaction mode.

Recommended for: high-concurrency production deployments that already use PgBouncer.

### Mode 3 — Transaction pooling everywhere + `poll_only = true`

No LISTEN/NOTIFY. Polling is the sole wakeup mechanism. Enqueue→pickup latency is bounded by `fetch_poll_interval` (default 1s).

```
everything → PgBouncer (transaction mode) → Postgres
```

Recommended for: managed-Postgres services that only expose transaction-pooled endpoints (Supabase pooler, some RDS Proxy configs).

## Alternatives considered

- **Support only Mode 1 (no PgBouncer)** — Rules out most high-concurrency production users. Unacceptable.
- **Support only Mode 3 (`poll_only` always)** — Wastes the low-latency wakeup path on users whose deployment supports it.
- **Detect pool mode automatically and switch** — Appealing but fragile. A pool-mode change in the user's PgBouncer config silently changes our behavior. Better to make the user declare intent via config.

## Consequences

- Docs must explain the three modes clearly with a decision flowchart. This is Phase 5 hardening work.
- `poll_only` is a first-class config flag, not a hidden knob. We add a startup-time warning if the worker coordinator appears to be connected via a transaction-pooled endpoint and `poll_only = false`.
- The worker coordinator's need for 2–5 direct connections is documented as part of the operational checklist. Users provisioning PgBouncer must carve out these connections.
- PgBouncer 1.21+ is the minimum supported version for Mode 2. We document this requirement and the `max_prepared_statements` setting.
- Future modes (e.g., a proxy that natively speaks pgwire and preserves LISTEN) can be added as additional ADRs without changing this one.
