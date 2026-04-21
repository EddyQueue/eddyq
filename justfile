set shell := ["bash", "-uc"]

# default: list available recipes
default:
    @just --list

# build everything (rust + node)
build:
    cargo build --workspace
    pnpm -r build

# format rust + node
fmt:
    cargo fmt --all
    pnpm -r exec prettier --write . || true

# lint everything
lint:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings
    pnpm -r lint

# run rust tests
test:
    cargo test --workspace

# run integration tests against a real postgres (testcontainers)
test-integration:
    cargo test --workspace --features integration -- --test-threads=1

# run benchmark harness (Phase 0 deliverable)
bench:
    cargo bench --workspace
    @echo "see bench-report.md"

# bring up local postgres for development
db-up:
    docker compose -f docker-compose.dev.yml up -d postgres

# tear down local postgres
db-down:
    docker compose -f docker-compose.dev.yml down

# audit dependencies
audit:
    cargo deny check
    cargo audit

# install all required dev tools (cargo-deny, cargo-audit, etc.)
install-dev-tools:
    cargo install cargo-deny cargo-audit cargo-release
    @echo "node side: pnpm install"
