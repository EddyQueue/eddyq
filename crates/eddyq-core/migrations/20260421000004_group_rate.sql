ALTER TABLE eddyq_groups
    ADD COLUMN rate_count         INTEGER,
    ADD COLUMN rate_period_ms     INTEGER,
    ADD COLUMN tokens             DOUBLE PRECISION NOT NULL DEFAULT 0,
    ADD COLUMN tokens_refilled_at TIMESTAMPTZ,
    ADD CONSTRAINT eddyq_groups_rate_check
        CHECK (
            (rate_count IS NULL AND rate_period_ms IS NULL)
            OR (rate_count > 0 AND rate_period_ms > 0)
        );
