-- Index for the claim's group walk when some subscribed queues are full.
--
-- The walk (fetch.rs, phase 2) finds distinct group keys with pending work by
-- seeking through eddyq_jobs_group (group_key, ...). queue is not in that
-- index, so once full or paused queues are excluded from the walk, every
-- pending row of a group that only has work in an excluded queue is fetched
-- from the heap and rejected. In production, with two capped queues at their
-- caps, that was thousands of rejected rows to find one claimable group on
-- every poll of every all-queue worker.
--
-- With queue leading, the walk seeks (queue, group_key) pairs within the open
-- queues only and never visits an excluded queue's rows.
--
-- Partial on pending grouped rows, so it stays small (only the live grouped
-- backlog). Built inside the migration transaction: writes to eddyq_jobs wait
-- for the build, which scans the heap once (about a second on a 400 MB table).
CREATE INDEX IF NOT EXISTS eddyq_jobs_group_queue
    ON eddyq_jobs (queue, group_key, scheduled_at)
    WHERE group_key IS NOT NULL AND state = 'pending';
