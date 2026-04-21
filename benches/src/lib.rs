//! eddyq-benches — cross-system benchmark harness.
//!
//! Compares eddyq against `sqlxmq` (Rust), `graphile-worker` (Node), and `BullMQ` (Redis)
//! across throughput, latency percentiles, memory under sustained load, and behavior
//! during Postgres failover.
//!
//! Phase 0 deliverable: the harness exists and is runnable before any queue logic
//! is written, so all subsequent design choices are bench-verified.

#![forbid(unsafe_code)]
#![allow(missing_docs)] // internal bench crate; not published, not user-facing

pub mod report;
pub mod runners;
pub mod scenarios;
