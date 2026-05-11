//! Per-job retention rule (BullMQ-style `removeOnComplete` /
//! `removeOnFail`). Redis-only feature — `PgBackend` ignores these fields,
//! `RedisBackend` honors them inline in `eddyq_complete` / `eddyq_fail`.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Retention semantics for a finalized job. Mirrors BullMQ's API exactly so
/// migration from BullMQ doesn't require relearning the knobs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum RetentionRule {
    /// `true` => drop immediately on finalize, `false` => keep forever.
    Bool(bool),
    /// Structured rule with optional age-based and count-based caps.
    Rule {
        /// Drop entries older than this many seconds.
        #[serde(skip_serializing_if = "Option::is_none")]
        age: Option<u64>,
        /// Keep at most the most recent N entries.
        #[serde(skip_serializing_if = "Option::is_none")]
        count: Option<u32>,
    },
}

impl RetentionRule {
    /// Convenience: drop on finalize.
    pub fn drop() -> Self {
        Self::Bool(true)
    }

    /// Convenience: keep forever.
    pub fn keep() -> Self {
        Self::Bool(false)
    }

    /// Keep the N most recent entries.
    pub fn keep_count(n: u32) -> Self {
        Self::Rule {
            age: None,
            count: Some(n),
        }
    }

    /// Keep entries newer than `age`.
    pub fn keep_age(age: Duration) -> Self {
        Self::Rule {
            age: Some(age.as_secs()),
            count: None,
        }
    }

    /// Combined: keep the most recent N entries, but drop any older than
    /// `age` regardless.
    pub fn keep_both(age: Duration, count: u32) -> Self {
        Self::Rule {
            age: Some(age.as_secs()),
            count: Some(count),
        }
    }

    /// JSON encoding suitable for the `remove_on_complete` /
    /// `remove_on_fail` field on `DynEnqueue`. Empty string when None at
    /// the call site.
    pub fn to_arg(rule: Option<&RetentionRule>) -> String {
        match rule {
            None => String::new(),
            Some(r) => serde_json::to_string(r).unwrap_or_default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_rules() {
        for r in [
            RetentionRule::drop(),
            RetentionRule::keep(),
            RetentionRule::keep_count(100),
            RetentionRule::keep_age(Duration::from_secs(86400)),
            RetentionRule::keep_both(Duration::from_secs(86400), 100),
        ] {
            let s = serde_json::to_string(&r).unwrap();
            let back: RetentionRule = serde_json::from_str(&s).unwrap();
            assert_eq!(r, back);
        }
    }
}
