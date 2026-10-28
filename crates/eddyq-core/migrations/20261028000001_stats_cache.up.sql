-- Cluster-wide cache of terminal job counts for `get_stats`.
--
-- `get_stats` used to GROUP BY (queue, state) over all of eddyq_jobs on every
-- call. Nearly every row is a finalized job kept for retention (in one
-- production cluster: 811k of 812k rows completed/cancelled/failed, a 385 MB
-- heap, ~360 ms per call), and every process polling queue-depth gauges plus
-- every open dashboard tab paid that scan independently.
--
-- Live counts (pending, running) stay exact per call: they ride the partial
-- indexes the runtime already keeps for fetching and heartbeats. Terminal
-- counts come from this table, recomputed by at most one caller per TTL under
-- a transaction-scoped advisory lock, so their cost no longer scales with the
-- number of callers. Both tables are new; nothing here touches eddyq_jobs.

CREATE TABLE IF NOT EXISTS eddyq_stats_cache (
    queue TEXT   NOT NULL,
    state TEXT   NOT NULL,
    count BIGINT NOT NULL,
    PRIMARY KEY (queue, state)
);

CREATE TABLE IF NOT EXISTS eddyq_stats_meta (
    id          SMALLINT    PRIMARY KEY CHECK (id = 1),
    computed_at TIMESTAMPTZ NOT NULL
);
