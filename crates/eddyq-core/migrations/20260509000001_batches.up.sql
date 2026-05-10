-- eddyq batches: groups N jobs and fires `on_complete` exactly once when every
-- job in the batch reaches a terminal state.

CREATE TABLE eddyq_batches (
    id                   BIGSERIAL   PRIMARY KEY,
    state                TEXT        NOT NULL DEFAULT 'pending',
    total                INTEGER     NOT NULL CHECK (total >= 0),
    completed            INTEGER     NOT NULL DEFAULT 0 CHECK (completed >= 0),
    failed               INTEGER     NOT NULL DEFAULT 0 CHECK (failed    >= 0),
    cancelled            INTEGER     NOT NULL DEFAULT 0 CHECK (cancelled >= 0),
    -- Serialized DynEnqueue (kind, payload, options). NULL = no callback.
    on_complete          JSONB,
    -- Claim marker: set atomically when total is reached, prevents double-fire.
    callback_enqueued_at TIMESTAMPTZ,
    metadata             JSONB       NOT NULL DEFAULT '{}'::JSONB,
    created_at           TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    finalized_at         TIMESTAMPTZ,
    CONSTRAINT eddyq_batches_state_check
        CHECK (state IN ('pending', 'complete')),
    CONSTRAINT eddyq_batches_counts_check
        CHECK (completed + failed + cancelled <= total)
);

-- FK added in three steps so VALIDATE can be split into its own migration
-- later for zero-downtime upgrades on populated eddyq_jobs tables.
ALTER TABLE eddyq_jobs ADD COLUMN batch_id BIGINT;

ALTER TABLE eddyq_jobs
    ADD CONSTRAINT eddyq_jobs_batch_id_fkey
    FOREIGN KEY (batch_id) REFERENCES eddyq_batches(id)
    ON DELETE SET NULL
    NOT VALID;

ALTER TABLE eddyq_jobs VALIDATE CONSTRAINT eddyq_jobs_batch_id_fkey;

CREATE INDEX eddyq_jobs_batch
    ON eddyq_jobs (batch_id)
    WHERE batch_id IS NOT NULL;

-- Partial index used by `fetch::cleanup` to reap finalized batches efficiently.
-- Pending batches (callback hasn't fired) are excluded so the index stays small.
CREATE INDEX eddyq_batches_finalized
    ON eddyq_batches (finalized_at)
    WHERE state = 'complete';

-- ─── Schedules: per-schedule queue ──────────────────────────────────────────
-- Schedule fires now route to a named queue, matching jobs and `enqueue`.
-- Without this, scheduled jobs always landed on `default`, so a worker pod
-- subscribed to a non-default queue would silently miss its scheduled work.
ALTER TABLE eddyq_schedules
    ADD COLUMN queue TEXT NOT NULL DEFAULT 'default';
