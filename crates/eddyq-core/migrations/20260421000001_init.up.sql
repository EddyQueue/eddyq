CREATE TABLE eddyq_jobs (
    id            BIGSERIAL PRIMARY KEY,
    kind          TEXT        NOT NULL,
    payload       JSONB       NOT NULL,
    state         TEXT        NOT NULL,
    priority      SMALLINT    NOT NULL DEFAULT 0,
    attempt       INTEGER     NOT NULL DEFAULT 0,
    max_attempts  INTEGER     NOT NULL DEFAULT 3,
    scheduled_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    heartbeat_at  TIMESTAMPTZ,
    worker_id     UUID,
    errors        JSONB       NOT NULL DEFAULT '[]'::JSONB,
    unique_key    TEXT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    completed_at  TIMESTAMPTZ,
    CONSTRAINT eddyq_jobs_state_check
        CHECK (state IN ('pending', 'running', 'completed', 'failed', 'scheduled', 'cancelled'))
);

CREATE INDEX eddyq_jobs_fetch
    ON eddyq_jobs (priority DESC, scheduled_at ASC, id ASC)
    WHERE state = 'pending';

CREATE INDEX eddyq_jobs_heartbeat
    ON eddyq_jobs (heartbeat_at)
    WHERE state = 'running';

CREATE UNIQUE INDEX eddyq_jobs_unique
    ON eddyq_jobs (kind, unique_key)
    WHERE unique_key IS NOT NULL AND state IN ('pending', 'running', 'scheduled');
