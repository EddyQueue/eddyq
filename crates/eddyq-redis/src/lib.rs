//! eddyq-redis — Redis Functions backend for the eddyq job queue.
//!
//! Status: **PR1 skeleton.** Every `Backend` method returns
//! `Error::Unsupported` until PR2 lands the Redis Functions library and the
//! hot-path implementations (enqueue, claim, complete, fail, sweep, leader).
//! PR3 adds the differentiators (groups, schedules, rate limits, list_jobs).
//!
//! See `/Users/remingtonstone/.claude/plans/i-want-to-add-dapper-crystal.md`
//! for the full plan.

#![forbid(unsafe_code)]

mod backend;
mod bootstrap;
mod error;
mod functions;
mod keys;
pub mod retention;

pub use backend::{RedisBackend, RedisConfig};
pub use error::Error;
pub use retention::RetentionRule;
