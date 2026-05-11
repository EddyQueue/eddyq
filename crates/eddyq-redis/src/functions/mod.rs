//! Embedded Redis Functions library + bootstrap loader.
//!
//! The library source is compiled into the binary via `include_str!` so the
//! eddyq-redis crate is self-contained — no separate file to ship. Loading
//! happens lazily on first FCALL ("function not loaded" → load → retry).

/// Library name used in `FUNCTION LOAD` / `FUNCTION LIST`. Bumping this
/// would let an old + new library coexist during a rolling upgrade — for
/// now we only have v1.
pub const LIBRARY_NAME: &str = "eddyq_v1";

/// Raw Lua source for the v1 function library. Embedded at compile time.
pub const LIBRARY_SOURCE: &str = include_str!("library.lua");

// Function names — the FCALL targets. Kept here as constants so typo'd
// names show up at compile time, not the first time the call hits Redis.
pub const FN_ENQUEUE: &str = "eddyq_enqueue";
pub const FN_ENQUEUE_MANY: &str = "eddyq_enqueue_many";
pub const FN_CLAIM: &str = "eddyq_claim";
pub const FN_HEARTBEAT: &str = "eddyq_heartbeat";
pub const FN_COMPLETE: &str = "eddyq_complete";
pub const FN_FAIL: &str = "eddyq_fail";
pub const FN_SWEEP_STALE: &str = "eddyq_sweep_stale";
pub const FN_PROMOTE_DELAYED: &str = "eddyq_promote_delayed";
pub const FN_RECLAIM_IN_FLIGHT: &str = "eddyq_reclaim_in_flight";
pub const FN_CANCEL: &str = "eddyq_cancel";
pub const FN_LEADER_TRY: &str = "eddyq_leader_try";
pub const FN_LEADER_RESIGN: &str = "eddyq_leader_resign";
pub const FN_GROUP_SET_CONCURRENCY: &str = "eddyq_group_set_concurrency";
pub const FN_GROUP_SET_PAUSED: &str = "eddyq_group_set_paused";
pub const FN_GROUP_SET_RATE: &str = "eddyq_group_set_rate";
pub const FN_GROUP_CLEAR_RATE: &str = "eddyq_group_clear_rate";
pub const FN_GROUP_GET: &str = "eddyq_group_get";
pub const FN_GROUP_LIST: &str = "eddyq_group_list";
pub const FN_SCHEDULE_UPSERT: &str = "eddyq_schedule_upsert";
pub const FN_SCHEDULE_REMOVE: &str = "eddyq_schedule_remove";
pub const FN_SCHEDULE_SET_ENABLED: &str = "eddyq_schedule_set_enabled";
pub const FN_SCHEDULE_LIST: &str = "eddyq_schedule_list";
pub const FN_SCHEDULE_DUE_LIST: &str = "eddyq_schedule_due_list";
pub const FN_SCHEDULE_FIRE: &str = "eddyq_schedule_fire";
pub const FN_SCHEDULE_SYNC_DIFF: &str = "eddyq_schedule_sync_diff";
pub const FN_QUEUE_SET_CONCURRENCY: &str = "eddyq_queue_set_concurrency";
pub const FN_QUEUE_SET_PAUSED: &str = "eddyq_queue_set_paused";
pub const FN_QUEUE_SET_TIMEOUT: &str = "eddyq_queue_set_timeout";
pub const FN_QUEUE_GET: &str = "eddyq_queue_get";
pub const FN_QUEUE_LIST: &str = "eddyq_queue_list";
pub const FN_GET_STATS: &str = "eddyq_get_stats";
pub const FN_LIST_JOBS: &str = "eddyq_list_jobs";
pub const FN_GROUP_SET_RULE: &str = "eddyq_group_set_rule";
pub const FN_GROUP_REMOVE_RULE: &str = "eddyq_group_remove_rule";
pub const FN_GROUP_LIST_RULES: &str = "eddyq_group_list_rules";
