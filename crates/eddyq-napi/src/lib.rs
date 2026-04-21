//! eddyq-napi — Node.js bindings for the eddyq client.
//!
//! Built into a `cdylib` consumed by the `@eddyq/queue` npm package via NAPI-RS.
//! Pre-alpha; surface area will land in Phase 3.

#![allow(clippy::missing_safety_doc)]

#[macro_use]
extern crate napi_derive;

/// Returns the crate version. Smoke-test export for the NAPI build pipeline.
#[napi]
#[must_use]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
