# eddyq benchmark harness

Phase 0 deliverable. Compares eddyq against:

- **sqlxmq** (Rust, Postgres)
- **graphile-worker** (Node, Postgres)
- **BullMQ** (Node, Redis)

...on identical workloads: throughput, p50/p99/p999 latency, memory under sustained load, and behavior during Postgres failover.

## Why a bench harness before a queue?

Because "this queue is fast" is not a claim, it's a measurement. Having the harness before the engine means every design choice (SKIP LOCKED vs advisory locks, polling cadence, NOTIFY debounce window) is bench-verified rather than vibes-based. Also: publishable numbers on day one of v1.0.

## Layout

```
benches/
├── Cargo.toml              # bench crate (not published)
├── src/
│   ├── lib.rs              # module tree
│   ├── bin/main.rs         # `eddyq-bench` orchestrator binary
│   ├── scenarios.rs        # load patterns shared across runners
│   ├── report.rs           # bench-report.md generator
│   └── runners/            # one adapter per system under test
│       ├── eddyq.rs
│       ├── sqlxmq.rs
│       ├── graphile.rs     # shells out to Node
│       └── bullmq.rs       # shells out to Node + Redis
├── benches/
│   └── enqueue.rs          # criterion micro-benchmarks (CI-sensitive)
└── README.md               # this file
```

## Running

```bash
# Bring up Postgres (and Redis, for BullMQ runs):
just db-up

# Run the full canonical scenario set:
cargo run --release --bin eddyq-bench -- run --scenario all --system all

# Run just one:
cargo run --release --bin eddyq-bench -- run --scenario throughput-10k --system eddyq

# Run criterion micro-benchmarks:
cargo bench -p eddyq-benches
```

Produces `bench-report.md` at the workspace root.

## Status

**Scaffolded only.** The runner stubs exist; real implementations land once `eddyq-core` (Phase 1) has a functioning queue engine. The `scenarios` module already encodes the canonical workload set, so the API shape is stable from the start.
