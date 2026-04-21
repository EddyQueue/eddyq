# Architecture Decision Records

Each major design choice in eddyq is captured as an ADR (Architecture Decision Record) so the reasoning is reviewable and future-you (and contributors) can see why things are the way they are.

## Format

Every ADR has the same shape:

```
# NNN — Title
Status: Accepted | Proposed | Superseded by NNN | Deprecated
Date: YYYY-MM-DD

## Context
What problem are we solving, and what constraints are in play?

## Decision
What did we decide?

## Alternatives considered
What else did we look at, and why did we not pick them?

## Consequences
What does this unlock? What does it cost? What are we now committed to?
```

## Index

| # | Title | Status |
|---|-------|--------|
| [001](001-async-runtime-tokio.md) | Async runtime: Tokio | Accepted |
| [002](002-db-driver-sqlx.md) | Database driver: sqlx | Accepted |
| [003](003-migrations-refinery.md) | Migrations: refinery | Accepted |
| [004](004-hybrid-wakeup.md) | Job wakeup: hybrid poll + LISTEN/NOTIFY | Accepted |
| [005](005-node-bindings-napi-rs.md) | Node bindings: NAPI-RS v3 | Accepted |
| [006](006-observability.md) | Observability: tracing + OpenTelemetry + metrics facade | Accepted |
| [007](007-workspace-layout.md) | Workspace shape: four Rust crates | Accepted |
| [008](008-rust-only-handlers-v1.md) | v1.0 handlers: Rust only | Accepted |
| [009](009-optional-partitioning.md) | Job table partitioning: available but not default | Accepted |
| [010](010-pgbouncer-modes.md) | Three supported PgBouncer deployment modes | Accepted |
