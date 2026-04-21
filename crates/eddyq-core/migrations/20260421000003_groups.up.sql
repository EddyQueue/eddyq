CREATE TABLE eddyq_groups (
    key             TEXT        PRIMARY KEY,
    running_count   INTEGER     NOT NULL DEFAULT 0 CHECK (running_count >= 0),
    max_concurrency INTEGER     NOT NULL DEFAULT 2147483647 CHECK (max_concurrency >= 0),
    paused          BOOLEAN     NOT NULL DEFAULT FALSE,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

ALTER TABLE eddyq_jobs ADD COLUMN group_key TEXT;

CREATE INDEX eddyq_jobs_group
    ON eddyq_jobs (group_key, priority DESC, scheduled_at ASC)
    WHERE group_key IS NOT NULL AND state = 'pending';
