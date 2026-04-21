# Contributing to eddyq

Thanks for your interest in eddyq! This project is in **Phase 0** of its v1.0 roadmap. Architecture is largely locked via [ADRs](docs/decisions/) — please read them before opening design-shaped PRs.

## Prerequisites

- **Rust** (pinned via `rust-toolchain.toml`; rustup will install automatically on first `cargo` invocation)
- **Node 20+** and **pnpm 10+** (for the Node packages and dashboard)
- **Docker** (for local Postgres via `just db-up`)
- **just** (`brew install just` or `cargo install just`)

One-time dev tooling install:

```bash
just install-dev-tools
pnpm install
```

## Common commands

```bash
just build           # cargo build --workspace + pnpm -r build
just test            # cargo test --workspace
just test-integration  # spins up Postgres via testcontainers
just lint            # fmt --check, clippy -D warnings, pnpm lint
just bench           # runs benchmark harness, emits bench-report.md
```

## Pull requests

1. **Read the ADRs.** If your PR contradicts a locked decision, open a discussion first — we'll need a new ADR.
2. **Add a changeset** for any user-visible change: `pnpm changeset` (Node side) or note it in the PR description (Rust side; we use `cargo-release` for changelogs).
3. **All CI green.** clippy `-D warnings`, rustfmt, cargo-deny, cargo-audit, and the integration suite must pass.
4. **No new dependencies without justification.** This is a library that ends up in users' production stacks; we keep the dependency footprint tight.

## Reporting bugs / requesting features

- Bugs: file an issue with the bug template, include reproducer + Postgres version + eddyq version.
- Features: file an issue with the feature template — but note that we're aggressively scoping v1.0 (see the [v1.0 non-goals](docs/decisions/) in the plan). Many "obvious" features are deliberately deferred.

## Security

See [SECURITY.md](SECURITY.md) for vulnerability disclosure.

## Code of conduct

See [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).
