//! Re-export of the core retention type. The canonical definition lives
//! in `eddyq_core::retention` so `DynEnqueue` and both backends can share
//! one shape; this module exists for backwards compatibility with code
//! that imports `eddyq_redis::RetentionRule`.

pub use eddyq_core::retention::RetentionRule;
