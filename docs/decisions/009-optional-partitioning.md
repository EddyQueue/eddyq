# 009 — Job table partitioning: available but not default
Status: Accepted
Date: 2026-04-21

## Context

`SELECT ... FOR UPDATE SKIP LOCKED` is the fetch primitive. It works well up to ~128 concurrent workers on a single table; past that, lock-walker contention causes CPU spikes and query waits measured in minutes. ([Netdata case study](https://www.netdata.cloud/academy/update-skip-locked/))

The mitigation is table partitioning: split `eddyq_jobs` into N partitions and assign workers to prefer a subset, reducing contention per table. The cost is operational complexity — partitioned tables, partition-aware queries, partition management (creating new ones, dropping old ones).

## Decision

- **v1 ships an unpartitioned schema by default.** Simpler onboarding, fine for the 90% of users below the contention cliff.
- **Partitioning is a documented upgrade path**, not a default. Phase 5 includes migration docs for converting an unpartitioned `eddyq_jobs` to a partitioned one.
- **Completed-jobs archival via partitioning by `created_at`** is the recommended pattern: drop whole partitions of old completed jobs instead of `DELETE FROM ... WHERE completed_at < ...`.

## Alternatives considered

- **Partitioning on by default** — Forces users to think about partition management on day one. Bad default for small deployments.
- **Partitioning never** — Caps the project's real-world ceiling at the SKIP LOCKED contention limit. Unacceptable long-term.
- **Sharding across multiple Postgres instances** — Different problem (cross-instance coordination), explicitly a v1.0 non-goal.

## Consequences

- Unpartitioned `eddyq_jobs` is the documented default. Recommended for deployments under ~100 concurrent workers or ~10k sustained jobs/sec.
- At higher scales, we document: (a) how to partition, (b) how to assign workers to partitions, (c) how to drop old completed partitions.
- Migration from unpartitioned to partitioned is documented but requires user-side downtime or a managed-migration plan. We don't attempt a zero-downtime automatic conversion in v1.
- Phase 0 benchmark harness runs at 50, 100, 200, and 400 worker counts to characterize the contention curve on unpartitioned tables; this informs the "when to partition" recommendation.
