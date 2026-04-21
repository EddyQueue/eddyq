use std::time::Duration;

use chrono::{DateTime, Utc};
use rand::Rng;

/// Exponential backoff with jitter.
///
/// Delay is `base * 2^(attempt-1)`, capped at `max`, plus up to 25% random jitter.
/// `attempt` is the attempt number that just failed (1-indexed).
pub fn backoff_delay(attempt: i32, base: Duration, max: Duration) -> Duration {
    let exp = u32::try_from(attempt.max(1) - 1).unwrap_or(0).min(20);
    let multiplier = 1u64 << exp.min(32); // 2^exp, safely
    let scaled = base.saturating_mul(u32::try_from(multiplier).unwrap_or(u32::MAX));
    let capped = scaled.min(max);

    let jitter_range = capped.as_millis() / 4;
    let jitter_ms = if jitter_range == 0 {
        0
    } else {
        rand::thread_rng().gen_range(0..u64::try_from(jitter_range).unwrap_or(u64::MAX))
    };
    capped + Duration::from_millis(jitter_ms)
}

pub fn backoff_until(attempt: i32, base: Duration, max: Duration) -> DateTime<Utc> {
    let delay = backoff_delay(attempt, base, max);
    Utc::now() + chrono::Duration::from_std(delay).unwrap_or(chrono::Duration::zero())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_exponentially_until_cap() {
        let base = Duration::from_secs(1);
        let max = Duration::from_secs(60);

        let d1 = backoff_delay(1, base, max);
        let d2 = backoff_delay(2, base, max);
        let d3 = backoff_delay(3, base, max);
        let d4 = backoff_delay(4, base, max);

        // attempt 1: 1s + up to 250ms jitter → 1000..=1250 ms
        assert!((1000..=1250).contains(&d1.as_millis()));
        // attempt 2: 2s + up to 500ms
        assert!((2000..=2500).contains(&d2.as_millis()));
        // attempt 3: 4s + up to 1s
        assert!((4000..=5000).contains(&d3.as_millis()));
        // attempt 4: 8s + up to 2s
        assert!((8000..=10_000).contains(&d4.as_millis()));
    }

    #[test]
    fn backoff_caps_at_max() {
        let base = Duration::from_secs(1);
        let max = Duration::from_secs(10);
        // 2^19 * 1s would be ~6 days without a cap; cap kicks in.
        let d = backoff_delay(20, base, max);
        // max + up to 25% jitter
        assert!((10_000..=12_500).contains(&d.as_millis()));
    }
}
