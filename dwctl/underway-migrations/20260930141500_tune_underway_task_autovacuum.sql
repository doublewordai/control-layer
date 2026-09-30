-- Vacuum and analyze underway.task after a fixed number of changes instead of a
-- fraction of the table.
--
-- underway.task is a queue that also keeps completed tasks for the retention
-- window, so it holds millions of rows while its live pending/in-progress set
-- is small. Postgres's default thresholds are a fraction of the whole table
-- (scale_factor 0.2), so autovacuum waits for over a million dead tuples.
-- Meanwhile every completed task leaves a dead entry at the front of
-- idx_task_claim, and the visibility map goes stale, so each claim walks those
-- entries through heap fetches. Claims then take minutes and the queue stops
-- draining; a manual VACUUM restores sub-second claims.
--
-- Fixed thresholds keep maintenance proportional to queue churn regardless of
-- how much history the table retains. The statement is metadata-only and takes
-- effect on the next autovacuum cycle.

ALTER TABLE underway.task SET (
    autovacuum_vacuum_scale_factor        = 0.0,
    autovacuum_vacuum_threshold           = 20000,
    autovacuum_vacuum_insert_scale_factor = 0.0,
    autovacuum_vacuum_insert_threshold    = 20000,
    autovacuum_analyze_scale_factor       = 0.0,
    autovacuum_analyze_threshold          = 20000
);
