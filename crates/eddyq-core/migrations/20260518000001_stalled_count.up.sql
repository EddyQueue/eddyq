-- Separate stall recovery from handler-throw retry budget.
--
-- `stalled_count` is incremented when a worker disappears mid-handler
-- (sweep_stale or reclaim_in_flight rescues the row). It's bounded by
-- `max_stalled_count`; once exceeded the row is moved to `failed`.
-- `attempt` is reserved for completed handler invocations (success or throw).
--
-- Both ADDs are metadata-only in PG 11+ (constant defaults), so no rewrite.

ALTER TABLE eddyq_jobs
    ADD COLUMN stalled_count     INTEGER NOT NULL DEFAULT 0,
    ADD COLUMN max_stalled_count INTEGER NOT NULL DEFAULT 1;
