use thiserror::Error;

/// Crate-local error type. Lifted into `eddyq_core::Error` (specifically
/// `Error::Backend(String)`) at the trait boundary so the runtime sees a
/// uniform `eddyq_core::Result`.
#[derive(Debug, Error)]
pub enum Error {
    #[error("redis error: {0}")]
    Redis(#[from] redis::RedisError),

    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("redis function library not loaded: {0}")]
    FunctionLibrary(String),

    #[error("backend protocol error: {0}")]
    Protocol(String),
}

impl From<Error> for eddyq_core::Error {
    fn from(value: Error) -> Self {
        eddyq_core::Error::Backend(value.to_string())
    }
}
