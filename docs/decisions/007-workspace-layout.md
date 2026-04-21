# 007 — Workspace shape: four Rust crates
Status: Accepted
Date: 2026-04-21

## Context

The eddyq codebase needs to serve at least four distinct consumers: the Rust worker engine (heavy), the Rust enqueue-only client (lightweight), a command-line tool, and the NAPI-RS Node bindings. Putting them in one crate makes every consumer pay the full dependency cost; splitting them too finely makes internal refactors expensive.

## Decision

Cargo workspace with four crates:

| Crate | Role | Key deps |
|-------|------|----------|
| `eddyq-core` | Queue engine, schema, migrations, fetch/claim/sweep logic | tokio, sqlx, refinery |
| `eddyq-client` | Enqueue + admin API only (no worker runtime) | tokio, sqlx, eddyq-core |
| `eddyq-cli` | `eddyq` binary (migrate, jobs, groups, drain) | eddyq-client, clap |
| `eddyq-napi` | NAPI-RS bindings → `@eddyq/queue` | eddyq-client, napi, napi-derive |

The pnpm workspace lives alongside in `packages/` with `@eddyq/queue`, `@eddyq/nestjs`, and `@eddyq/dashboard`.

## Alternatives considered

- **Single `eddyq` crate** — Simpler, but every consumer pulls in the full worker runtime even if they only enqueue. Also: `eddyq-napi` must be a `cdylib`; you can't cleanly mix crate types in one Cargo package.
- **Six or seven crates** (separating e.g. `eddyq-schema`, `eddyq-wakeup`, `eddyq-worker`) — Premature. We don't know where the real module boundaries will emerge. Start coarse, split when actual friction appears.
- **Three crates** (merging `eddyq-core` and `eddyq-client`) — Works, but then every user of "just the enqueue API" pulls in the worker engine.

## Consequences

- `eddyq-napi` depends on `eddyq-client`, not `eddyq-core`. The NAPI shared library stays small.
- `eddyq-client` is the public enqueue surface. Most users will add `eddyq-client` to their `Cargo.toml`, not `eddyq-core`.
- Internal refactors that cross crate boundaries cost more than intra-crate ones. Accepted.
- Publishing is done with `cargo-release` in dependency order: `eddyq-core` → `eddyq-client` → `eddyq-cli`. `eddyq-napi` is not published to crates.io; it ships via the `@eddyq/queue` npm package.
