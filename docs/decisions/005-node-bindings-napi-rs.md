# 005 — Node bindings: NAPI-RS v3
Status: Accepted
Date: 2026-04-21

## Context

The eddyq wedge in the Node ecosystem depends on bindings that feel native: `pnpm add @eddyq/queue` should just work, with a small binary that loads fast on startup and supports async operations without blocking the Node event loop.

## Decision

Use **NAPI-RS v3** to build the bindings in `crates/eddyq-napi`. Ship prebuilt binaries for:

- `darwin-arm64`, `darwin-x64`
- `linux-x64-gnu`, `linux-arm64-gnu`
- `linux-x64-musl`
- `win32-x64`

Ship a **WASM fallback** for unsupported platforms.

Recommend **pnpm** or **yarn** over **npm** in all documentation, due to npm's known lockfile bug with platform-specific `optionalDependencies`.

## Alternatives considered

- **Neon** — Mature, well-documented, but smaller maintainer base and slower async story (thread-pool based). NAPI-RS's `tokio_rt` integration is more native.
- **WASM / WASI only** — Platform-universal, no prebuilt-binary distribution complexity, but performance is measurably worse for database-heavy workloads (no native tokio-postgres; WASM-sql is immature).
- **FFI + hand-rolled bindings** — Maximum control, maximum maintenance burden. Not worth it for a library.
- **No Rust bindings; pure Node client** — Defeats the project premise. The point is to reuse the Rust engine from Node.

## Consequences

- We depend on NAPI-RS's CI tooling for the prebuilt-binary matrix. NAPI-RS v3 has strong GitHub Actions templates for this.
- npm is documented as "supported but not recommended." The failure mode ("missing binary for platform") is user-visible and confusing, and the workaround is pnpm/yarn.
- We cannot easily share the eddyq worker runtime across Node and Rust because v1 is Rust-handlers-only (ADR 008). The bindings expose only client-side operations (enqueue, admin, query).
- WASM fallback adds build complexity but covers FreeBSD, unusual libc targets, and CI environments that pull arbitrary platforms.
