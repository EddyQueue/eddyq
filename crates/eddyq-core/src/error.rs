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

    /// The current backend doesn't support this operation. Returned by
    /// non-Postgres backends for Postgres-only methods (e.g. transactional
    /// enqueue) and by the Redis backend before its differentiator phase
    /// lands (groups, schedules, list_jobs).
    #[error("operation not supported by this backend: {0}")]
    Unsupported(String),

    /// Generic backend-side failure (Redis I/O, protocol, function-load
    /// errors). Postgres errors land in `Database` via the sqlx From impl.
    #[error("backend error: {0}")]
    Backend(String),
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

/// Largest `message`, in bytes, stored in a job's error log entry. Handler
/// errors routinely embed whole SQL statements or response bodies, and every
/// retry appends another entry, so an uncapped field turns one failing job
/// into tens of kilobytes on either backend.
pub const MAX_ERROR_MESSAGE_BYTES: usize = 2_048;

/// Largest `stack`, in bytes, stored in a job's error log entry. The top
/// frames carry the signal, so the tail is what gets cut.
pub const MAX_ERROR_STACK_BYTES: usize = 4_096;

/// Most recent entries kept in a job's error log. Each append trims older
/// entries: on Postgres in the `errors` UPDATEs in `fetch.rs`, on Redis in
/// the Lua `push_error` helper, whose literal a test in `eddyq-redis` pins to
/// this value.
pub const MAX_ERROR_ENTRIES: usize = 10;

impl HandlerFailure {
    pub fn from_message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            ..Default::default()
        }
    }

    /// Build the JSON entry stored in `eddyq_jobs.errors` for this failure.
    /// `message` and `stack` are capped at [`MAX_ERROR_MESSAGE_BYTES`] and
    /// [`MAX_ERROR_STACK_BYTES`]; every backend's handler failures pass
    /// through here on the way to storage.
    pub fn as_error_entry(&self) -> serde_json::Value {
        let mut obj = serde_json::Map::new();
        obj.insert("at".into(), serde_json::json!(chrono::Utc::now()));
        obj.insert(
            "message".into(),
            serde_json::Value::String(truncate_for_storage(&self.message, MAX_ERROR_MESSAGE_BYTES)),
        );
        if let Some(name) = &self.name {
            obj.insert("name".into(), serde_json::Value::String(name.clone()));
        }
        if let Some(stack) = &self.stack {
            obj.insert(
                "stack".into(),
                serde_json::Value::String(truncate_for_storage(stack, MAX_ERROR_STACK_BYTES)),
            );
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

/// Cut `s` to at most `max_bytes` bytes on a UTF-8 character boundary. When
/// anything is dropped, a `...[truncated N bytes]` marker replaces the tail
/// and counts toward `max_bytes`, so the result fits the budget (for any
/// budget wider than the marker itself).
fn truncate_for_storage(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_owned();
    }
    // At most `s.len()` bytes are dropped, so a marker sized for that is the
    // widest this input can need.
    let reserve = truncation_marker(s.len()).len();
    let mut cut = max_bytes.saturating_sub(reserve);
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = String::with_capacity(max_bytes);
    out.push_str(&s[..cut]);
    out.push_str(&truncation_marker(s.len() - cut));
    out
}

fn truncation_marker(dropped: usize) -> String {
    format!("...[truncated {dropped} bytes]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_fields_are_stored_verbatim() {
        let failure = HandlerFailure {
            message: "boom".into(),
            name: Some("TypeError".into()),
            stack: Some("TypeError: boom\n    at x".into()),
            directive: None,
        };
        let entry = failure.as_error_entry();
        assert_eq!(entry["message"], "boom");
        assert_eq!(entry["stack"], "TypeError: boom\n    at x");
        assert_eq!(entry["name"], "TypeError");
    }

    #[test]
    fn fields_at_the_cap_are_untouched() {
        let s = "a".repeat(MAX_ERROR_MESSAGE_BYTES);
        assert_eq!(truncate_for_storage(&s, MAX_ERROR_MESSAGE_BYTES), s);
    }

    #[test]
    fn long_fields_are_capped_with_marker() {
        let failure = HandlerFailure {
            message: "m".repeat(10_000),
            stack: Some("s".repeat(50_000)),
            ..Default::default()
        };
        let entry = failure.as_error_entry();

        let message = entry["message"].as_str().unwrap();
        assert!(message.len() <= MAX_ERROR_MESSAGE_BYTES);
        let kept = message.bytes().take_while(|&b| b == b'm').count();
        assert!(kept > MAX_ERROR_MESSAGE_BYTES - 32, "cap is filled");
        assert_eq!(
            &message[kept..],
            format!("...[truncated {} bytes]", 10_000 - kept)
        );

        let stack = entry["stack"].as_str().unwrap();
        assert!(stack.len() <= MAX_ERROR_STACK_BYTES);
        let kept = stack.bytes().take_while(|&b| b == b's').count();
        assert_eq!(
            &stack[kept..],
            format!("...[truncated {} bytes]", 50_000 - kept)
        );
    }

    #[test]
    fn truncation_lands_on_a_char_boundary() {
        // 3-byte characters, so most byte budgets fall mid-character.
        let ch = '\u{6f22}';
        let s = ch.to_string().repeat(1_000);
        for max in 100..103 {
            let out = truncate_for_storage(&s, max);
            assert!(out.len() <= max, "{} > {max}", out.len());
            let kept = out.chars().take_while(|&c| c == ch).count();
            assert_eq!(
                out,
                format!(
                    "{}...[truncated {} bytes]",
                    ch.to_string().repeat(kept),
                    s.len() - kept * ch.len_utf8()
                )
            );
        }
    }
}
