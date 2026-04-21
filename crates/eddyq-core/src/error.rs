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
