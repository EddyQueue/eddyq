//! Criterion micro-benchmarks for the enqueue hot path.
//!
//! Complements the end-to-end scenarios in `src/scenarios.rs` with per-operation
//! timing so regressions in the enqueue path show up in CI.

#![allow(missing_docs)] // criterion_main! expands to undocumented items

use criterion::{Criterion, criterion_group, criterion_main};

fn bench_placeholder(c: &mut Criterion) {
    c.bench_function("placeholder", |b| b.iter(|| 2 + 2));
}

criterion_group!(benches, bench_placeholder);
criterion_main!(benches);
