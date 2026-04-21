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

    /// Group this job belongs to for concurrency / rate-limit purposes. Jobs
    /// sharing a `group_key` are subject to the per-group `max_concurrency` limit
    /// (set via `Queue::set_group_concurrency`). Returning `None` means the job
    /// is not group-limited.
    fn group_key(&self) -> Option<String> {
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
