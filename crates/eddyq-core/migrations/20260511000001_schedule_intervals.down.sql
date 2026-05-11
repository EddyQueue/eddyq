-- Reverse: drop CHECK, restore NOT NULL on cron_expr, drop interval_ms.
-- This will fail if any interval-only rows still exist (cron_expr IS NULL);
-- callers downgrading must first delete or convert those schedules.

ALTER TABLE eddyq_schedules
    DROP CONSTRAINT IF EXISTS eddyq_schedules_trigger_chk;

ALTER TABLE eddyq_schedules
    ALTER COLUMN cron_expr SET NOT NULL;

ALTER TABLE eddyq_schedules
    DROP COLUMN IF EXISTS interval_ms;
