use serde::{Serialize, de::DeserializeOwned};
use uuid::Uuid;

pub type JobId = i64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    Pending,
    Running,
    Completed,
    Failed,
    Scheduled,
    Cancelled,
}

impl JobState {
    pub fn as_str(self) -> &'static str {
        match self {
            JobState::Pending => "pending",
            JobState::Running => "running",
            JobState::Completed => "completed",
            JobState::Failed => "failed",
            JobState::Scheduled => "scheduled",
            JobState::Cancelled => "cancelled",
        }
    }
}

/// Default queue name. Jobs with no explicit queue go here, workers subscribe here by
/// default.
pub const DEFAULT_QUEUE: &str = "default";

pub trait Job: Serialize + DeserializeOwned + Send + Sync + 'static {
    const KIND: &'static str;

    fn max_attempts(&self) -> i32 {
        3
    }

    fn priority(&self) -> i16 {
        0
    }

    fn unique_key(&self) -> Option<String> {
        None
    }

    /// Which named queue this job should be enqueued on. Workers subscribe to
    /// queues via `QueueBuilder::subscribe_to`. Default is `"default"`.
    ///
    /// Use named queues for *routing pools* — separate worker processes for
    /// urgent vs default vs heavy workloads. Use `group_key` for concurrency
    /// limiting. The two are orthogonal.
    fn queue(&self) -> &'static str {
        DEFAULT_QUEUE
    }

    /// Group this job belongs to for concurrency / rate-limit purposes. Jobs
    /// sharing a `group_key` are subject to the per-group `max_concurrency` limit
    /// (set via `Queue::set_group_concurrency`). Returning `None` means the job
    /// is not group-limited.
    fn group_key(&self) -> Option<String> {
        None
    }

    /// Tags attached to this job for filtering in dashboards / admin queries.
    /// Stored in a GIN-indexed TEXT[] column; query with `tags @> '{urgent}'`.
    fn tags(&self) -> Vec<String> {
        vec![]
    }

    /// Arbitrary per-job metadata — distinct from `payload`, which is the
    /// input to the handler. Use this for trace IDs, user context, source
    /// attribution — anything the handler doesn't need but admin tooling does.
    fn metadata(&self) -> Option<serde_json::Value> {
        None
    }
}

#[derive(Debug, Clone)]
pub struct JobContext {
    pub id: JobId,
    pub kind: String,
    pub attempt: i32,
    pub max_attempts: i32,
    pub worker_id: Uuid,
}
