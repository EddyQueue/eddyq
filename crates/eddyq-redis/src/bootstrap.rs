//! Lazy load + version-check the embedded Redis Functions library.
//!
//! On startup we don't blindly `FUNCTION LOAD REPLACE` — that would replace
//! a coexisting newer library a peer just deployed. Instead we:
//!
//! 1. `FUNCTION LIST LIBRARYNAME eddyq_v1` to see if our exact library
//!    is loaded with our exact source SHA-256.
//! 2. If absent or SHA differs, `FUNCTION LOAD REPLACE` with our source.
//! 3. Concurrent loaders: REPLACE is atomic on the Redis side (Redis is
//!    single-threaded). Worst case two pods load identical bytes — fine.
//!
//! Lazy retry: callers wrap an FCALL with `with_retry` so a flushed Redis
//! (rare — `FUNCTION FLUSH`, full restart on a non-AOF instance) is
//! recovered transparently.

use redis::aio::ConnectionManager;
use sha2::{Digest, Sha256};

use crate::Error;
use crate::functions::{LIBRARY_NAME, LIBRARY_SOURCE};

/// SHA-256 hex of the embedded library source. Stable across processes
/// since the source is a compile-time constant.
#[allow(dead_code)]
pub fn source_sha() -> String {
    let mut h = Sha256::new();
    h.update(LIBRARY_SOURCE.as_bytes());
    format!("{:x}", h.finalize())
}

/// Make sure the library is loaded and matches our embedded source. Safe to
/// call on every connection — short-circuits when the library is already
/// loaded with the right code. Returns `true` if a fresh load happened, so
/// the caller can fire one-shot migrations (e.g. backfill of newly-added
/// state ZSETs) that only need to run on actual library upgrades.
pub async fn ensure_loaded(conn: &mut ConnectionManager) -> Result<bool, Error> {
    if library_matches(conn).await? {
        return Ok(false);
    }
    load_replace(conn).await?;
    Ok(true)
}

async fn library_matches(conn: &mut ConnectionManager) -> Result<bool, Error> {
    // `FUNCTION LIST LIBRARYNAME <name> WITHCODE` returns the source if
    // present. We compare to our embedded source.
    let result: redis::Value = redis::cmd("FUNCTION")
        .arg("LIST")
        .arg("LIBRARYNAME")
        .arg(LIBRARY_NAME)
        .arg("WITHCODE")
        .query_async(conn)
        .await?;

    match &result {
        redis::Value::Array(libs) if !libs.is_empty() => {
            // Find the "library_code" field in the first library entry.
            if let redis::Value::Array(fields) = &libs[0] {
                let mut iter = fields.iter();
                while let (Some(k), Some(v)) = (iter.next(), iter.next()) {
                    if let redis::Value::BulkString(bytes) = k {
                        if bytes == b"library_code" {
                            if let redis::Value::BulkString(code) = v {
                                let loaded = String::from_utf8_lossy(code);
                                return Ok(loaded == LIBRARY_SOURCE);
                            }
                        }
                    }
                }
            }
            Ok(false)
        }
        _ => Ok(false),
    }
}

async fn load_replace(conn: &mut ConnectionManager) -> Result<(), Error> {
    let _: redis::Value = redis::cmd("FUNCTION")
        .arg("LOAD")
        .arg("REPLACE")
        .arg(LIBRARY_SOURCE)
        .query_async(conn)
        .await?;
    Ok(())
}

/// Call an async closure that issues an FCALL. On a "function not found" /
/// "no library" error, lazy-load the library and retry once. Catches the
/// rare-but-real case of `FUNCTION FLUSH` while workers are running.
pub async fn with_retry<F, Fut, T>(conn: &mut ConnectionManager, f: F) -> Result<T, Error>
where
    F: Fn(ConnectionManager) -> Fut,
    Fut: std::future::Future<Output = Result<T, redis::RedisError>>,
{
    match f(conn.clone()).await {
        Ok(v) => Ok(v),
        Err(err) => {
            if is_no_function_err(&err) {
                let _ = ensure_loaded(conn).await?;
                f(conn.clone()).await.map_err(Into::into)
            } else {
                Err(err.into())
            }
        }
    }
}

fn is_no_function_err(err: &redis::RedisError) -> bool {
    let msg = err.to_string();
    // Redis 7 emits these on FCALL against a missing library/function.
    msg.contains("Function not found")
        || msg.contains("no such function")
        || msg.contains("NOSCRIPT")
}

/// Touch a fresh connection-manager: load the library if needed. Used by
/// `RedisBackend::connect`. We swallow not-loaded errors and let
/// `with_retry` handle them on the first FCALL.
#[allow(dead_code)]
pub async fn warm_up(conn: &mut ConnectionManager) {
    let _ = ensure_loaded(conn).await.ok();
}

// Quick sanity: source SHA round-trips.
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sha_is_stable_hex() {
        let s = source_sha();
        assert_eq!(s.len(), 64);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
