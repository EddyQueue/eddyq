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
