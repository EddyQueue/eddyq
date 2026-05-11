//! Key-naming helpers. All keys for a given line use the hash-tag
//! `{<line>}` so they map to a single Redis Cluster slot, allowing
//! multi-key Lua to run inside one Function call.
//!
//! Keep this module in sync with the Lua helpers in
//! `src/functions/library.lua`.

/// Build the hash-tag prefix for a line. e.g. `prefix("main")` → `"{main}"`.
/// This is what Lua sees as `KEYS[1]`.
pub fn prefix(line: &str) -> String {
    format!("{{{}}}", line)
}

#[allow(dead_code)]
pub fn job(line: &str, id: i64) -> String {
    format!("{}:job:{}", prefix(line), id)
}

#[allow(dead_code)]
pub fn errors(line: &str, id: i64) -> String {
    format!("{}:job:{}:errors", prefix(line), id)
}

#[allow(dead_code)]
pub fn wait(line: &str, queue: &str) -> String {
    format!("{}:wait:{}", prefix(line), queue)
}

#[allow(dead_code)]
pub fn delayed(line: &str) -> String {
    format!("{}:delayed", prefix(line))
}

#[allow(dead_code)]
pub fn active(line: &str) -> String {
    format!("{}:active", prefix(line))
}

#[allow(dead_code)]
pub fn completed(line: &str) -> String {
    format!("{}:completed", prefix(line))
}

#[allow(dead_code)]
pub fn failed(line: &str) -> String {
    format!("{}:failed", prefix(line))
}

#[allow(dead_code)]
pub fn cancelled(line: &str) -> String {
    format!("{}:cancelled", prefix(line))
}

#[allow(dead_code)]
pub fn unique(line: &str, key: &str) -> String {
    format!("{}:unique:{}", prefix(line), key)
}

#[allow(dead_code)]
pub fn leader(line: &str, role: &str) -> String {
    format!("{}:leader:{}", prefix(line), role)
}

pub fn wakeup_channel(line: &str) -> String {
    format!("{}:wakeup", prefix(line))
}

pub fn resign_channel(line: &str) -> String {
    format!("{}:resign", prefix(line))
}
