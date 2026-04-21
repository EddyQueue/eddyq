# 001 — Async runtime: Tokio
Status: Accepted
Date: 2026-04-21

## Context

eddyq is an async Rust project. The Rust ecosystem has two production async runtimes in significant use (Tokio and async-std, with smol as a third), and our choice constrains every downstream library we can pull in.

## Decision

Use **Tokio** as the sole async runtime.

## Alternatives considered

- **async-std** — Declining momentum; most library authors target Tokio first.
- **smol** — Smaller, pleasant, but the ecosystem for Postgres queue components (sqlx with runtime-tokio, tokio-util's CancellationToken, NAPI-RS's `tokio_rt` feature) assumes Tokio.
- **Runtime-agnostic via executor-trait** — Genuinely portable but the abstraction cost shows up everywhere, and every library we want to depend on picks Tokio anyway.

## Consequences

- Every `async fn` runs on Tokio. We can use `#[tokio::main]`, `tokio::spawn`, `tokio::select!`, `tokio::time::sleep`, `CancellationToken`, and the standard Tokio primitives freely.
- sqlx is enabled with the `runtime-tokio` feature.
- NAPI-RS uses its `tokio_rt` integration so async Rust work is visible to Node's event loop.
- We cannot add dependencies that require async-std or smol without a runtime bridge, which we will refuse to do.
