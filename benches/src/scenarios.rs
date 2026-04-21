//! Load patterns shared across all runners.
//!
//! Each scenario describes a reproducible workload (N jobs, M workers, payload shape,
//! failure injection). Runners execute the same scenario independently; results are
//! collated by `report`.

use std::time::Duration;

/// A reproducible benchmark scenario.
#[derive(Debug, Clone)]
pub struct Scenario {
    pub name: &'static str,
    pub n_jobs: usize,
    pub n_workers: usize,
    pub payload_size_bytes: usize,
    pub timeout: Duration,
    pub kind: ScenarioKind,
}

/// The shape of work a scenario models.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScenarioKind {
    /// Sustained enqueue + dequeue at steady state.
    Throughput,
    /// Single-job latency measurement at low concurrency.
    Latency,
    /// Group-concurrency correctness under load (Phase 2+).
    GroupConcurrency,
    /// Behavior while Postgres primary is failed over.
    Failover,
    /// SKIP LOCKED contention curve at 50/100/200/400 workers (ADR 009 calibration).
    ContentionCurve,
}

/// The canonical benchmark set for each eddyq release.
#[must_use]
pub fn release_scenarios() -> Vec<Scenario> {
    vec![
        Scenario {
            name: "throughput-10k",
            n_jobs: 10_000,
            n_workers: 10,
            payload_size_bytes: 256,
            timeout: Duration::from_secs(120),
            kind: ScenarioKind::Throughput,
        },
        Scenario {
            name: "latency-low-contention",
            n_jobs: 1_000,
            n_workers: 1,
            payload_size_bytes: 256,
            timeout: Duration::from_secs(60),
            kind: ScenarioKind::Latency,
        },
        Scenario {
            name: "contention-curve",
            n_jobs: 50_000,
            n_workers: 200,
            payload_size_bytes: 256,
            timeout: Duration::from_secs(300),
            kind: ScenarioKind::ContentionCurve,
        },
    ]
}
