ALTER TABLE eddyq_groups DROP CONSTRAINT IF EXISTS eddyq_groups_rate_check;
ALTER TABLE eddyq_groups
    DROP COLUMN IF EXISTS rate_count,
    DROP COLUMN IF EXISTS rate_period_ms,
    DROP COLUMN IF EXISTS tokens,
    DROP COLUMN IF EXISTS tokens_refilled_at;
