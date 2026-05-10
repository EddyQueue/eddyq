ALTER TABLE eddyq_schedules DROP COLUMN IF EXISTS queue;
DROP INDEX IF EXISTS eddyq_batches_finalized;
DROP INDEX IF EXISTS eddyq_jobs_batch;
ALTER TABLE eddyq_jobs DROP CONSTRAINT IF EXISTS eddyq_jobs_batch_id_fkey;
ALTER TABLE eddyq_jobs DROP COLUMN IF EXISTS batch_id;
DROP TABLE IF EXISTS eddyq_batches;
