-- eddyq batches: native fan-in primitive.
--
-- A batch groups N jobs and tracks their collective terminal-state count, so
-- that an `on_complete` callback fires exactly once when every job in the batch
-- has reached a terminal state (completed, failed, or cancelled). This avoids
-- the per-app counter table workaround (e.g. nexmail's klaviyo_events_backfill_runs).

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

ALTER TABLE eddyq_jobs
    ADD COLUMN batch_id BIGINT REFERENCES eddyq_batches(id) ON DELETE SET NULL;

-- Serves: terminal-transition lookup of "still-running jobs in batch X" and
-- the maintenance fallback sweep that scans for batches whose counters reached
-- total but whose callback never fired (should be empty in steady state).
CREATE INDEX eddyq_jobs_batch
    ON eddyq_jobs (batch_id)
    WHERE batch_id IS NOT NULL;
