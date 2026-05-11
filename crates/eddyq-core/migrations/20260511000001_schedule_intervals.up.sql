-- Add interval-driven schedule support alongside cron-driven schedules.
-- Each row is now exactly one of:
--   * cron-driven     (cron_expr  IS NOT NULL, interval_ms IS NULL)
--   * interval-driven (cron_expr  IS NULL,     interval_ms IS NOT NULL)
-- The CHECK enforces this; existing rows all have cron_expr set, so the
-- constraint is satisfied for them.
--
-- All three statements are metadata-only or trivial table scans on a
-- small admin table — kept in one migration to keep upgrade ops simple.

ALTER TABLE eddyq_schedules
    ADD COLUMN interval_ms BIGINT;

ALTER TABLE eddyq_schedules
    ALTER COLUMN cron_expr DROP NOT NULL;

ALTER TABLE eddyq_schedules
    ADD CONSTRAINT eddyq_schedules_trigger_chk
    CHECK ((cron_expr IS NOT NULL) <> (interval_ms IS NOT NULL));

-- Index audit:
-- The existing partial index `eddyq_schedules_due ON (next_run_at) WHERE
-- enabled` continues to cover the tick query for both cron- and interval-
-- driven rows — they share the same `next_run_at` column and `enabled`
-- predicate. No new index is needed for this change.
--
-- Note for very small intervals (e.g. `every: 50ms`): each fire UPDATEs
-- `next_run_at`, which is the indexed column, so the update can't be
-- HOT. Every fire produces a dead tuple. For typical schedule counts
-- (dozens to hundreds of rows) this is negligible — autovacuum keeps up.
-- High-frequency schedules at thousand-row scale should monitor table
-- bloat or use a coarser cadence.
