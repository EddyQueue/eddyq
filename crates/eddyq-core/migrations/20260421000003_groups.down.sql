DROP INDEX IF EXISTS eddyq_jobs_group;
ALTER TABLE eddyq_jobs DROP COLUMN IF EXISTS group_key;
DROP TABLE IF EXISTS eddyq_groups;
