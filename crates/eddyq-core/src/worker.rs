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

type Handler = Arc<
    dyn Fn(serde_json::Value, JobContext) -> BoxFuture<'static, JobResult> + Send + Sync + 'static,
>;

#[derive(Default)]
pub struct WorkerRegistry {
    handlers: HashMap<&'static str, Handler>,
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
                worker.perform(job, ctx).await
            })
        });
        self.handlers.insert(J::KIND, handler);
    }

    pub(crate) fn get(&self, kind: &str) -> Result<Handler> {
        self.handlers
            .get(kind)
            .cloned()
            .ok_or_else(|| Error::UnknownKind(kind.to_owned()))
    }

    pub fn kinds(&self) -> Vec<&'static str> {
        self.handlers.keys().copied().collect()
    }
}
