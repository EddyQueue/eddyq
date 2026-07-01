-- Reserve in-page free space on the hot counter tables so their updates go
-- HOT (heap-only tuples).
--
-- eddyq_groups / eddyq_leader are tiny tables whose rows are UPDATEd on every
-- job claim + completion (running_count, tokens) or heartbeat. At the default
-- fillfactor (100) pages are packed full, so each counter tick writes a new
-- tuple on a new page plus a PK index entry -- observed in production as tens
-- of millions of updates on a few hundred rows and the top autovacuum load in
-- the cluster. None of the mutated columns are indexed, so with free space
-- reserved the updates become in-page HOT rewrites: no index churn, a
-- fraction of the WAL and (on Aurora) billed storage I/O.
--
-- eddyq_jobs rows are also updated several times across their lifecycle
-- (claim, heartbeat, finalize) but the table is large and rows are deleted by
-- retention, so a milder setting keeps the space overhead small.
--
-- fillfactor only applies as pages are (re)filled; existing pages adopt it
-- through normal churn and vacuum. Operators wanting the full effect
-- immediately can VACUUM FULL the two small tables (sub-second) out-of-band;
-- intentionally not done here because migrations run in a transaction and
-- VACUUM cannot.

ALTER TABLE eddyq_groups SET (fillfactor = 70);
ALTER TABLE eddyq_leader SET (fillfactor = 70);
ALTER TABLE eddyq_queues SET (fillfactor = 70);
ALTER TABLE eddyq_jobs   SET (fillfactor = 90);

-- Leader election state is pure transient coordination (same pattern as
-- River's river_leader): heartbeats rewrite the row every few seconds, and
-- losing it just triggers a re-election -- claim_leadership() upserts with
-- ON CONFLICT, so an empty table is a normal cold start. UNLOGGED skips WAL
-- for those heartbeats entirely. Caveats by design: unlogged tables are
-- truncated on crash recovery and not visible on streaming/Aurora read
-- replicas -- both fine for a single-row election token that only the
-- writer touches. The rewrite this ALTER performs is one row.

ALTER TABLE eddyq_leader SET UNLOGGED;
