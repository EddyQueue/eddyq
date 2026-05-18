//! eddyq-core — Postgres-backed job queue engine.

#![forbid(unsafe_code)]
#![allow(missing_docs)]

pub mod backend;
pub mod batch;
pub mod enqueue;
pub mod error;
pub mod fetch;
pub mod group;
pub mod job;
pub mod leader;
pub mod migrate;
pub mod named_queue;
pub mod queue;
pub mod retention;
pub mod retry;
pub(crate) mod runtime;
pub mod schedule;
pub mod stats;
pub mod worker;

pub use async_trait::async_trait;
pub use backend::{Backend, BackendCaps, CleanState, PgBackend};
pub use batch::{BatchEnqueueResult, BatchOptions};
pub use enqueue::{BulkEnqueueResult, DynEnqueue, EnqueueOptions, EnqueueResult};
pub use error::{Directive, Error, HandlerFailure, JobResult, Result};
pub use job::{DEFAULT_QUEUE, Job, JobContext, JobId, JobState};
pub use queue::{DrainOutcome, Queue, QueueBuilder, QueueConfig, ShutdownMode};
pub use retention::RetentionRule;
pub use worker::{Worker, WorkerRegistry};
