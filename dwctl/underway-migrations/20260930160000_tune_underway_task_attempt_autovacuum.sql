-- Vacuum and analyze underway.task_attempt after a fixed number of changes
-- instead of a fraction of the table.
--
-- Every claim that considers a stalled task counts that task's attempts, and
-- every claim and completion writes an attempt row. The table keeps the
-- attempts of completed tasks for the retention window, so it holds millions
-- of rows while the attempts that matter are recent. With the default
-- fraction-of-table thresholds, dead rows and a stale visibility map build up
-- for hours between runs and the per-task attempt counts fall back to heap
-- fetches. Same thresholds as underway.task, which churns in step with it.
--
-- ALTER TABLE ... SET waits for a running vacuum of the same table; fail after
-- a few seconds (a regular autovacuum yields after deadlock_timeout) instead
-- of holding the migration behind an anti-wraparound vacuum.
--
-- Metadata-only change; takes effect on the next autovacuum cycle.
SET LOCAL lock_timeout = '5s';

ALTER TABLE underway.task_attempt SET (
    autovacuum_vacuum_scale_factor        = 0.0,
    autovacuum_vacuum_threshold           = 20000,
    autovacuum_vacuum_insert_scale_factor = 0.0,
    autovacuum_vacuum_insert_threshold    = 20000,
    autovacuum_analyze_scale_factor       = 0.0,
    autovacuum_analyze_threshold          = 20000
);
