# 008 — v1.0 handlers: Rust only
Status: Accepted
Date: 2026-04-21

## Context

The most-requested feature for a Node-facing queue is "let me write my job handlers in TypeScript." Supporting that in a Rust engine is architecturally non-trivial:

- **Embed V8 / Node-API in the Rust binary** — Lots of surface area, fragile across Node versions, process-wide global state.
- **Rust worker spawns a Node subprocess per handler invocation** — Clean isolation, but slow, and IPC overhead dominates for fast jobs.
- **Separate `@eddyq/worker` Node CLI that pulls jobs from Postgres directly** — Duplicates the worker runtime in Node, defeats much of the Rust engine's value.

None of these are cheap. All of them are design choices that should be informed by real usage, not early speculation.

## Decision

**v1.0 supports Rust-language handlers only.** Node packages are enqueue + admin only. Handlers must be written in Rust (`impl Worker<T>`).

## Alternatives considered

- **Ship Node handlers in v1** — Too expensive for the first release. Locks in a design we can't change.
- **Punt Node handlers permanently** — Closes off the Node worker-runtime market. We leave room for it in v1.x.
- **Require users to write a tiny Rust shim that calls into their Node code over IPC** — Architecturally honest, DX is bad.

## Consequences

- Node packages (`@eddyq/queue`, `@eddyq/nestjs`) expose only `enqueue()`, admin, and query methods in v1. Users run Rust workers.
- This limits the initial addressable market to teams willing to run a Rust binary alongside their Node app. That's a smaller set than "any NestJS app," but it's the early-adopter set — teams that already run Postgres, operate Kubernetes, and care enough about queue correctness to consider a new tool.
- Example NestJS repo for Phase 3 demonstrates the split: NestJS enqueues via `@eddyq/queue`, Rust worker consumes.
- v1.x revisit: decide based on user demand between V8 embedding and subprocess models. Open an ADR at that time.
