DROP INDEX IF EXISTS eddyq_jobs_batch;
ALTER TABLE eddyq_jobs DROP COLUMN IF EXISTS batch_id;
DROP TABLE IF EXISTS eddyq_batches;
