# 002 — Database driver: sqlx
Status: Accepted
Date: 2026-04-21

## Context

eddyq is Postgres-backed; the database driver choice determines how much compile-time safety we get, how transactional enqueue-in-user-transaction works, and how easy it is for downstream users to share their connection pool with us.

## Decision

Use **sqlx** (with the `runtime-tokio`, `postgres`, `uuid`, `chrono`, and `json` features) as the sole database driver in `eddyq-core`.

For transactional enqueue, accept anything that implements sqlx's `Executor` trait. Add feature-flagged integrations for `tokio-postgres` and `diesel-async` handles in Phase 2.

## Alternatives considered

- **tokio-postgres** — Lower-level, no DSL, supports query pipelining (sqlx does not). Lighter dependency footprint. Rejected because (a) users expect compile-time query verification from modern Rust DB libraries, (b) transactional enqueue with sqlx's `Executor` trait is the cleanest path to "here's my transaction, insert a job into it."
- **diesel-async** — Mature ORM with strong typed-query support. Heavier and more opinionated than we need; also less common in the Rust job-queue ecosystem (sqlxmq and graphile_worker_rs both use sqlx).
- **SeaORM** — Higher-level ORM on top of sqlx. Unnecessary ceremony for a queue engine that writes maybe a dozen queries total.

## Consequences

- Compile-time verified queries via `sqlx::query!` and `sqlx::query_as!`. CI either has access to a live database or uses `sqlx::offline` with checked-in metadata (`.sqlx/`). We default to offline mode in CI.
- Users enqueueing in their own transactions pass `&mut Transaction<'_, Postgres>` (sqlx) directly. Behind feature flags, we accept `tokio-postgres::Transaction` and `diesel_async::AsyncPgConnection` and internally bridge to the shared fetch/update SQL.
- We inherit sqlx's prepared-statement behavior, which matters for the PgBouncer deployment story (see ADR 010) — PgBouncer 1.21+ is required for transaction-mode pooling with prepared statements.
- No query pipelining in v1 (sqlx does not pipeline). If benchmarking shows this is the bottleneck, we revisit.
