# eddyq benchmarks

Throughput + latency numbers for the Redis and Postgres backends. Two
harnesses live here:

- **`benches/throughput.rs`** — single-process end-to-end harness. Runs
  three phases (single enqueue, bulk enqueue, end-to-end drain) against
  one or both backends and prints per-second numbers.
- **`benches/enqueue.rs`, `benches/claim.rs`** — Criterion micro-benches
  for the Postgres path. Useful for spotting regressions in the SQL path.

## Throughput harness

```bash
docker compose -f ../docker-compose.dev.yml up -d postgres redis

DATABASE_URL=postgres://eddyq:eddyq@localhost:5433/eddyq_dev \
REDIS_URL=redis://127.0.0.1:6381 \
cargo run --release -p eddyq-benches --bin throughput
```

### Knobs

| Env var | Default | Notes |
|---|---|---|
| `BENCH_JOBS` | 10000 | Total jobs per phase |
| `BENCH_WORKERS` | 16 | Per-process worker concurrency for the drain phase |
| `BENCH_BULK_BATCH` | 200 | Items per `enqueue_many` call |
| `BENCH_BACKENDS` | `redis,pg` | Comma list — set to one to skip the other |

### What each phase measures

| Phase | What it isolates |
|---|---|
| **single enqueue** | Per-op latency under sequential one-at-a-time enqueue. Reports total throughput + p50/p99 µs latency. |
| **bulk enqueue** | `enqueue_many` calls of `BENCH_BULK_BATCH` items. Headline ingest number. |
| **end-to-end drain** | Enqueue → worker handler returns → completed. Measures the realistic worker-pool throughput, no-op handler. |

## Reference numbers

Captured on an M-class laptop, dev compose stack (loopback Postgres + Redis),
no-op handler, no network. **Your numbers will vary** — these are an
order-of-magnitude reference, not a guarantee.

### Baseline (10k jobs, 16 workers, batch 200)

| Backend | single enqueue | bulk enqueue | drain | single latency |
|---|---|---|---|---|
| Redis | 6,473/s | **64,470/s** | 6,073/s | p50 139µs · p99 518µs |
| PG | 1,620/s | 51,521/s | 4,313/s | p50 554µs · p99 1,578µs |

### Medium (25k jobs, 32 workers, batch 500)

| Backend | single enqueue | bulk enqueue | drain | single latency |
|---|---|---|---|---|
| Redis | 6,140/s | **73,618/s** | 11,017/s | p50 150µs · p99 341µs |
| PG | 1,641/s | 47,350/s | 5,196/s | p50 577µs · p99 1,185µs |

### Heavy Redis-only (50k jobs, 64 workers, batch 500)

| Backend | single enqueue | bulk enqueue | drain | single latency |
|---|---|---|---|---|
| Redis | 6,369/s | 72,399/s | **19,219/s** | p50 149µs · p99 288µs |

## Takeaways

- **Bulk enqueue maxes around 70k jobs/sec on Redis.** A single
  `FCALL eddyq_enqueue_many` runs cjson + INCR + multiple ZADDs/HSETs
  inside one Lua block — the ceiling is Redis's single-threaded loop.
  Sharding across `line` hash-tags (one queue = one line = one slot)
  scales this horizontally on Redis Cluster.
- **Single-enqueue latency is ~4× lower on Redis** (p50 ~150µs vs ~560µs)
  and the p99 gap is wider. Round-trip overhead dominates Postgres
  because every enqueue is a transaction.
- **Drain scales nearly linearly with workers on Redis** (6k → 11k → 19k
  jobs/sec at 16 → 32 → 64 workers). Postgres flattens past ~32 workers —
  `FOR UPDATE SKIP LOCKED` contention on the same fetch batch.
- **PG bulk enqueue is competitive** (~50k/s vs Redis ~70k/s). The
  `UNNEST` INSERT is fast; the gap shows up in drain, not ingest.

## When to pick which

- **Pure Redis throughput workload (webhooks fan-out, ephemeral fan-in,
  cache-line jobs):** Redis backend. ~3–5× ingest, ~2–5× drain at scale.
- **Transactional enqueue (`enqueueInTx`), strong durability,
  batch fan-in (`enqueue_batch`):** Postgres backend. Redis can't offer
  enqueue-in-transaction with an external system.
- **Mixed workload:** `EddyqApp` — route per queue. See
  `examples/redis-basic/multi.mjs` for the wire-up.

## Reproducing

```bash
# Baseline
BENCH_JOBS=10000 BENCH_WORKERS=16 BENCH_BULK_BATCH=200 \
  cargo run --release -p eddyq-benches --bin throughput

# Heavy Redis
BENCH_BACKENDS=redis BENCH_JOBS=50000 BENCH_WORKERS=64 BENCH_BULK_BATCH=500 \
  cargo run --release -p eddyq-benches --bin throughput

# Latency-only (small sample)
BENCH_JOBS=2000 BENCH_WORKERS=8 BENCH_BACKENDS=redis \
  cargo run --release -p eddyq-benches --bin throughput
```

The harness flushes its `bench-<pid>-…` line/schema between phases so
prior runs don't skew the numbers.

## Caveats

- Handlers are no-op (`Ok(())`). Real handler time dominates the drain
  phase in production — these numbers are an **upper bound** on what the
  queue plumbing itself can sustain.
- Single-tokio-task enqueueing. Multi-producer numbers (N tasks pumping
  in parallel) would be higher. Open a follow-up if you want a
  concurrent-producer variant.
- Localhost network — no TLS, no managed Redis hop. Add ~1ms per round
  trip in a real deployment; single-enqueue numbers shrink the most.
- PG bench uses the default `eddyq_dev` schema. The Criterion benches
  isolate themselves with `SCHEMA bench`; the throughput harness
  `TRUNCATE`s between phases.
