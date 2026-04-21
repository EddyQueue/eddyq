# 003 — Migrations: refinery
Status: Accepted
Date: 2026-04-21

## Context

eddyq ships its own schema (`eddyq_jobs`, `eddyq_groups`, `eddyq_schedules`, etc.). Users need a way to apply these migrations reliably on startup or via CLI, and ideally without installing a separate tool.

## Decision

Use **refinery** with migrations embedded at compile time via `refinery::embed_migrations!`. The `eddyq` CLI exposes `eddyq migrate` which runs them against the configured database.

## Alternatives considered

- **sqlx-migrate** / **sqlx-cli** — Works, but requires either (a) users install `sqlx-cli` separately, or (b) we re-invent migration-file embedding ourselves. refinery solves both in one crate.
- **Generate and publish SQL, let users embed in their existing migration tool** (the "Django-style" approach of `showmigrations`/`dbmate`) — More work for users; wrong default for a batteries-included library. We may still offer raw SQL export as a Phase 5 nicety.
- **Custom migration runner** — Not worth it. Migration correctness is a solved problem.

## Consequences

- Migrations live in `crates/eddyq-core/src/migrations/` and are embedded in the published crate. No external files needed at runtime.
- Migration history is tracked in a `refinery_schema_history` table. If users later want eddyq migrations to coexist with their own (e.g., sqlx-managed) migrations, they can either run `eddyq migrate` separately or export the raw SQL.
- Downgrading is not supported. Forward-only migrations are documented.
- Users who truly don't want us running migrations on their database can disable embedded migrations via a feature flag and manage the schema themselves.
