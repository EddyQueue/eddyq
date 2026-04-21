use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use futures_util::future::BoxFuture;

use crate::{
    error::{Error, JobResult, Result},
    job::{Job, JobContext},
};

#[async_trait]
pub trait Worker<J: Job>: Send + Sync + 'static {
    async fn perform(&self, job: J, ctx: JobContext) -> JobResult;
}

/// Dispatcher signature: given a JSON payload and a JobContext, return a
/// future that resolves to `Ok(result_value)` on success or `Err(...)` on
/// failure. `result_value` is persisted to `eddyq_jobs.result`; trait-based
/// Rust workers produce `Value::Null` here since their perform() returns `()`.
pub(crate) type Handler = Arc<
    dyn Fn(serde_json::Value, JobContext)
        -> BoxFuture<'static, JobResult<serde_json::Value>>
    + Send + Sync + 'static,
>;

#[derive(Default)]
pub struct WorkerRegistry {
    handlers: HashMap<String, Handler>,
}

impl WorkerRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register<J, W>(&mut self, worker: W)
    where
        J: Job,
        W: Worker<J> + 'static,
    {
        let worker = Arc::new(worker);
        let handler: Handler = Arc::new(move |payload, ctx| {
            let worker = worker.clone();
            Box::pin(async move {
                let job: J = serde_json::from_value(payload)?;
                worker.perform(job, ctx).await.map(|_| serde_json::Value::Null)
            })
        });
        self.handlers.insert(J::KIND.to_owned(), handler);
    }

    /// Register a handler keyed by string `kind`. Use this when you don't have
    /// a compile-time `Job` trait impl — e.g. from language bindings where the
    /// handler is a foreign function (Node, Python). The handler receives the
    /// raw JSON payload and a `JobContext` and returns a future resolving to a
    /// result `Value` (stored in `eddyq_jobs.result`) or an error.
    pub fn register_dyn<F, Fut>(&mut self, kind: impl Into<String>, f: F)
    where
        F: Fn(serde_json::Value, JobContext) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = JobResult<serde_json::Value>> + Send + 'static,
    {
        let handler: Handler = Arc::new(move |payload, ctx| Box::pin(f(payload, ctx)));
        self.handlers.insert(kind.into(), handler);
    }

    pub(crate) fn get(&self, kind: &str) -> Result<Handler> {
        self.handlers
            .get(kind)
            .cloned()
            .ok_or_else(|| Error::UnknownKind(kind.to_owned()))
    }

    pub fn kinds(&self) -> Vec<String> {
        self.handlers.keys().cloned().collect()
    }
}
