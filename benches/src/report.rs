//! Markdown report generation for the benchmark harness.
//!
//! Consumes `RunStats` from each (system, scenario) cell and emits `bench-report.md`
//! at the workspace root. The report is committed alongside every release so users
//! can see how eddyq compares to alternatives on identical workloads.

/// Render a bench-report.md given a set of results.
#[must_use]
pub fn render(_results: &[()]) -> String {
    String::from(
        "# eddyq benchmark report\n\n\
         _Harness scaffolded in Phase 0; real results land once eddyq-core Phase 1 is done._\n",
    )
}
