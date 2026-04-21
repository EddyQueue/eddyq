-- eddyq initial schema.

-- ─── Jobs ──────────────────────────────────────────────────────────────────
CREATE TABLE eddyq_jobs (
    id            BIGSERIAL   PRIMARY KEY,
    queue         TEXT        NOT NULL DEFAULT 'default',
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
    result        JSONB,
    unique_key    TEXT,
    group_key     TEXT,
    tags          TEXT[]      NOT NULL DEFAULT '{}',
    metadata      JSONB       NOT NULL DEFAULT '{}'::JSONB,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    finalized_at  TIMESTAMPTZ,
    CONSTRAINT eddyq_jobs_state_check
        CHECK (state IN ('pending', 'running', 'completed', 'failed', 'scheduled', 'cancelled'))
);

-- Serves: fetch claim path (all pending, ordered). Queue is the leading column
-- so workers subscribed to a subset of queues can index-scan efficiently.
CREATE INDEX eddyq_jobs_fetch
    ON eddyq_jobs (queue, priority DESC, scheduled_at ASC, id ASC)
    WHERE state = 'pending';

-- Serves: fastlane claim (ungrouped pending).
CREATE INDEX eddyq_jobs_fetch_ungrouped
    ON eddyq_jobs (queue, priority DESC, scheduled_at ASC, id ASC)
    WHERE state = 'pending' AND group_key IS NULL;

-- Serves: per-group claim path.
CREATE INDEX eddyq_jobs_group
    ON eddyq_jobs (group_key, priority DESC, scheduled_at ASC)
    WHERE group_key IS NOT NULL AND state = 'pending';

-- Serves: sweeper (stale running jobs).
CREATE INDEX eddyq_jobs_heartbeat
    ON eddyq_jobs (heartbeat_at)
    WHERE state = 'running';

-- Serves: unique-job dedup via ON CONFLICT.
CREATE UNIQUE INDEX eddyq_jobs_unique
    ON eddyq_jobs (kind, unique_key)
    WHERE unique_key IS NOT NULL AND state IN ('pending', 'running', 'scheduled');

-- Serves: dashboard / admin list-by-kind.
CREATE INDEX eddyq_jobs_kind ON eddyq_jobs (kind);

-- Serves: recent completions / failures, archival queries.
CREATE INDEX eddyq_jobs_finalized
    ON eddyq_jobs (finalized_at DESC)
    WHERE state IN ('completed', 'failed', 'cancelled');

-- Serves: filter by tag in dashboards ("show me all jobs tagged 'urgent'").
CREATE INDEX eddyq_jobs_tags ON eddyq_jobs USING GIN (tags);

-- ─── Named queues (cross-process concurrency tracking) ────────────────────
-- Tracks running_count for each named queue so a cap of 10 on "integrations"
-- applies across all worker replicas, not per-process.
CREATE TABLE eddyq_queues (
    name               TEXT        PRIMARY KEY,
    running_count      INTEGER     NOT NULL DEFAULT 0 CHECK (running_count >= 0),
    max_concurrency    INTEGER     NOT NULL DEFAULT 2147483647 CHECK (max_concurrency >= 0),
    paused             BOOLEAN     NOT NULL DEFAULT FALSE,
    -- Default per-job timeout in ms. NULL = no timeout (River-compatible default).
    default_timeout_ms INTEGER CHECK (default_timeout_ms IS NULL OR default_timeout_ms > 0),
    created_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- ─── Groups ────────────────────────────────────────────────────────────────
CREATE TABLE eddyq_groups (
    key                TEXT             PRIMARY KEY,
    running_count      INTEGER          NOT NULL DEFAULT 0 CHECK (running_count >= 0),
    max_concurrency    INTEGER          NOT NULL DEFAULT 2147483647 CHECK (max_concurrency >= 0),
    paused             BOOLEAN          NOT NULL DEFAULT FALSE,
    rate_count         INTEGER,
    rate_period_ms     INTEGER,
    tokens             DOUBLE PRECISION NOT NULL DEFAULT 0,
    tokens_refilled_at TIMESTAMPTZ,
    created_at         TIMESTAMPTZ      NOT NULL DEFAULT NOW(),
    updated_at         TIMESTAMPTZ      NOT NULL DEFAULT NOW(),
    CONSTRAINT eddyq_groups_rate_check
        CHECK (
            (rate_count IS NULL AND rate_period_ms IS NULL)
            OR (rate_count > 0 AND rate_period_ms > 0)
        )
);

-- ─── Pattern-based group rules ─────────────────────────────────────────────
CREATE TABLE eddyq_group_rules (
    pattern         TEXT        PRIMARY KEY,
    max_concurrency INTEGER,
    rate_count      INTEGER,
    rate_period_ms  INTEGER,
    priority        INTEGER     NOT NULL DEFAULT 0,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT eddyq_group_rules_rate_check
        CHECK (
            (rate_count IS NULL AND rate_period_ms IS NULL)
            OR (rate_count > 0 AND rate_period_ms > 0)
        ),
    CONSTRAINT eddyq_group_rules_something_check
        CHECK (max_concurrency IS NOT NULL OR rate_count IS NOT NULL)
);

-- ─── Cron schedules ────────────────────────────────────────────────────────
CREATE TABLE eddyq_schedules (
    name         TEXT        PRIMARY KEY,
    kind         TEXT        NOT NULL,
    payload      JSONB       NOT NULL,
    cron_expr    TEXT        NOT NULL,
    next_run_at  TIMESTAMPTZ NOT NULL,
    last_run_at  TIMESTAMPTZ,
    enabled      BOOLEAN     NOT NULL DEFAULT TRUE,
    priority     SMALLINT    NOT NULL DEFAULT 0,
    max_attempts INTEGER     NOT NULL DEFAULT 3,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX eddyq_schedules_due ON eddyq_schedules (next_run_at) WHERE enabled;
