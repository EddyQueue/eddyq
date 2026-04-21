//! eddyq-core — Postgres-backed job queue engine.

#![forbid(unsafe_code)]
#![allow(missing_docs)]

pub mod enqueue;
pub mod error;
pub mod fetch;
pub mod group;
pub mod job;
pub mod migrate;
pub mod named_queue;
pub mod queue;
pub mod retry;
mod runtime;
pub mod schedule;
pub mod stats;
pub mod worker;

pub use async_trait::async_trait;
pub use enqueue::{BulkEnqueueResult, DynEnqueue, EnqueueOptions, EnqueueResult};
pub use error::{Directive, Error, HandlerFailure, JobResult, Result};
pub use job::{Job, JobContext, JobId, JobState};
pub use queue::{Queue, QueueBuilder, QueueConfig};
pub use worker::{Worker, WorkerRegistry};
