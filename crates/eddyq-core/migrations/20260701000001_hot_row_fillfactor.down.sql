-- Restore the default fillfactor (100). Existing pages keep whatever free
-- space they have; new pages pack full again.

ALTER TABLE eddyq_groups RESET (fillfactor);
ALTER TABLE eddyq_leader RESET (fillfactor);
ALTER TABLE eddyq_queues RESET (fillfactor);
ALTER TABLE eddyq_jobs   RESET (fillfactor);

ALTER TABLE eddyq_leader SET LOGGED;
