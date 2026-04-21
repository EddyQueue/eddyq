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

-- ─── SQL enqueue functions ─────────────────────────────────────────────────
-- Expose the enqueue path to non-Rust callers so Node/Python/Ruby can
-- participate in transactional enqueue through their own DB client. The
-- Rust path in `eddyq_core::enqueue::enqueue_dyn` does three things:
--   1. INSERT into eddyq_jobs with ON CONFLICT DO NOTHING
--   2. If inserted AND group_key IS NOT NULL: materialize an eddyq_groups row
--      from the best-matching eddyq_group_rules pattern
--   3. If inserted AND scheduled_at <= NOW(): pg_notify('eddyq_job', '')
-- These functions mirror that logic exactly.
--
-- Example (Prisma / Drizzle / pg inside an app transaction):
--   BEGIN;
--     INSERT INTO invoices (...) VALUES (...);
--     SELECT eddyq_enqueue('send.receipt', '{"invoiceId":42}'::jsonb);
--   COMMIT;



-- Internal helper: mirrors Rust's group::materialize_from_rule.
CREATE OR REPLACE FUNCTION eddyq_materialize_group_from_rule(p_key TEXT)
RETURNS VOID AS $$
BEGIN
    INSERT INTO eddyq_groups (
        key, max_concurrency, paused, rate_count, rate_period_ms, tokens, tokens_refilled_at
    )
    SELECT
        p_key,
        COALESCE(r.max_concurrency, 2147483647),
        FALSE,
        r.rate_count,
        r.rate_period_ms,
        COALESCE(r.rate_count::double precision, 0),
        CASE WHEN r.rate_count IS NOT NULL THEN NOW() END
      FROM eddyq_group_rules r
     WHERE p_key LIKE REPLACE(REPLACE(r.pattern, '*', '%'), '?', '_')
  ORDER BY r.priority DESC, LENGTH(r.pattern) DESC
     LIMIT 1
    ON CONFLICT (key) DO NOTHING;
END;
$$ LANGUAGE plpgsql;

-- Single-job enqueue. Returns the new job id, or NULL on unique_key conflict.
CREATE OR REPLACE FUNCTION eddyq_enqueue(
    p_kind          TEXT,
    p_payload       JSONB,
    p_queue         TEXT        DEFAULT 'default',
    p_priority      SMALLINT    DEFAULT 0,
    p_max_attempts  INTEGER     DEFAULT 3,
    p_scheduled_at  TIMESTAMPTZ DEFAULT NOW(),
    p_unique_key    TEXT        DEFAULT NULL,
    p_group_key     TEXT        DEFAULT NULL,
    p_tags          TEXT[]      DEFAULT ARRAY[]::text[],
    p_metadata      JSONB       DEFAULT '{}'::jsonb
) RETURNS BIGINT AS $$
DECLARE
    v_id      BIGINT;
    v_due_now BOOLEAN := p_scheduled_at <= NOW();
BEGIN
    INSERT INTO eddyq_jobs (
        kind, payload, state, priority, max_attempts, scheduled_at,
        unique_key, group_key, tags, metadata, queue
    )
    VALUES (
        p_kind, p_payload, 'pending', p_priority, p_max_attempts, p_scheduled_at,
        p_unique_key, p_group_key, p_tags, p_metadata, p_queue
    )
    ON CONFLICT DO NOTHING
    RETURNING id INTO v_id;

    IF v_id IS NULL THEN
        RETURN NULL;
    END IF;

    IF p_group_key IS NOT NULL THEN
        PERFORM eddyq_materialize_group_from_rule(p_group_key);
    END IF;

    IF v_due_now THEN
        PERFORM pg_notify('eddyq_job', '');
    END IF;

    RETURN v_id;
END;
$$ LANGUAGE plpgsql;

-- Bulk enqueue. Input is a JSONB array of job objects. Returns
-- { inserted: N, skipped: N } aggregate counts.
CREATE OR REPLACE FUNCTION eddyq_enqueue_many(p_items JSONB)
RETURNS JSONB AS $$
DECLARE
    v_inserted INTEGER := 0;
    v_skipped  INTEGER := 0;
    v_row_id   BIGINT;
    v_item     JSONB;
    v_any_due  BOOLEAN := FALSE;
    v_groups   TEXT[] := ARRAY[]::text[];
    v_gk       TEXT;
BEGIN
    IF p_items IS NULL OR jsonb_typeof(p_items) <> 'array' THEN
        RAISE EXCEPTION 'eddyq_enqueue_many: p_items must be a JSONB array';
    END IF;

    FOR v_item IN SELECT * FROM jsonb_array_elements(p_items) LOOP
        INSERT INTO eddyq_jobs (
            kind, payload, state, priority, max_attempts, scheduled_at,
            unique_key, group_key, tags, metadata, queue
        )
        VALUES (
            v_item ->> 'kind',
            v_item -> 'payload',
            'pending',
            COALESCE((v_item ->> 'priority')::smallint, 0),
            COALESCE((v_item ->> 'max_attempts')::integer, (v_item ->> 'maxAttempts')::integer, 3),
            COALESCE((v_item ->> 'scheduled_at')::timestamptz, (v_item ->> 'scheduledAt')::timestamptz, NOW()),
            v_item ->> 'unique_key',
            v_item ->> 'group_key',
            COALESCE(
                ARRAY(SELECT jsonb_array_elements_text(v_item -> 'tags')),
                ARRAY[]::text[]
            ),
            COALESCE(v_item -> 'metadata', '{}'::jsonb),
            COALESCE(v_item ->> 'queue', 'default')
        )
        ON CONFLICT DO NOTHING
        RETURNING id INTO v_row_id;

        IF v_row_id IS NOT NULL THEN
            v_inserted := v_inserted + 1;
            IF COALESCE((v_item ->> 'scheduled_at')::timestamptz, NOW()) <= NOW() THEN
                v_any_due := TRUE;
            END IF;
            v_gk := v_item ->> 'group_key';
            IF v_gk IS NOT NULL AND NOT (v_gk = ANY(v_groups)) THEN
                v_groups := array_append(v_groups, v_gk);
            END IF;
        ELSE
            v_skipped := v_skipped + 1;
        END IF;
    END LOOP;

    FOREACH v_gk IN ARRAY v_groups LOOP
        PERFORM eddyq_materialize_group_from_rule(v_gk);
    END LOOP;

    IF v_any_due AND v_inserted > 0 THEN
        PERFORM pg_notify('eddyq_job', '');
    END IF;

    RETURN jsonb_build_object('inserted', v_inserted, 'skipped', v_skipped);
END;
$$ LANGUAGE plpgsql;

COMMENT ON FUNCTION eddyq_enqueue(TEXT, JSONB, TEXT, SMALLINT, INTEGER, TIMESTAMPTZ, TEXT, TEXT, TEXT[], JSONB) IS
  'Transactional enqueue for non-Rust callers. Call from inside your own BEGIN/COMMIT to atomically tie a job to a domain write. Returns the new job id, or NULL on unique_key conflict.';
COMMENT ON FUNCTION eddyq_enqueue_many(JSONB) IS
  'Bulk transactional enqueue. Accepts a JSONB array of job objects. Returns { inserted, skipped }.';
