use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("migration error: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),

    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("no worker registered for job kind `{0}`")]
    UnknownKind(String),

    #[error("invalid cron expression: {0}")]
    Cron(String),

    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    #[error("queue already running")]
    AlreadyRunning,

    #[error("queue not running")]
    NotRunning,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

pub type JobResult<T = ()> = std::result::Result<T, anyhow::Error>;

/// Structured failure info emitted by a handler. Language bindings wrap their
/// native error (JS `Error`, Python `Exception`, etc.) into this so the queue
/// can store `name` / `stack` alongside the message and optionally act on a
/// retry directive. Rust handlers that just return `Err(anyhow!(...))` still
/// work — those land as `HandlerFailure { message: <str>, .. }` via an implicit
/// wrap in the runtime.
#[derive(Debug, Clone, Default)]
pub struct HandlerFailure {
    pub message: String,
    pub name: Option<String>,
    pub stack: Option<String>,
    pub directive: Option<Directive>,
}

/// Retry directive the handler can request via its rejection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Directive {
    /// Mark the job failed permanently — skip the default retry schedule.
    Cancel,
    /// Retry at `now + delay` instead of the exponential-backoff default.
    Retry { delay_ms: u64 },
}

impl std::fmt::Display for HandlerFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.name {
            Some(name) => write!(f, "{name}: {}", self.message),
            None => write!(f, "{}", self.message),
        }
    }
}

impl std::error::Error for HandlerFailure {}

impl HandlerFailure {
    pub fn from_message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            ..Default::default()
        }
    }

    /// Build the JSON entry stored in `eddyq_jobs.errors` for this failure.
    pub fn as_error_entry(&self) -> serde_json::Value {
        let mut obj = serde_json::Map::new();
        obj.insert("at".into(), serde_json::json!(chrono::Utc::now()));
        obj.insert(
            "message".into(),
            serde_json::Value::String(self.message.clone()),
        );
        if let Some(name) = &self.name {
            obj.insert("name".into(), serde_json::Value::String(name.clone()));
        }
        if let Some(stack) = &self.stack {
            obj.insert("stack".into(), serde_json::Value::String(stack.clone()));
        }
        if let Some(dir) = &self.directive {
            match dir {
                Directive::Cancel => {
                    obj.insert("directive".into(), serde_json::json!("cancel"));
                }
                Directive::Retry { delay_ms } => {
                    obj.insert("directive".into(), serde_json::json!("retry"));
                    obj.insert("retryDelayMs".into(), serde_json::json!(*delay_ms));
                }
            }
        }
        serde_json::Value::Object(obj)
    }
}
