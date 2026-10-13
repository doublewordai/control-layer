-- no-transaction
--
-- Driving set for queries that look for batches finishing after their
-- deadline, such as SLA-miss monitoring:
--
--   WHERE deleted_at IS NULL AND completed_at > expires_at
--     AND completed_at > NOW() - INTERVAL '1 hour'
--
-- and the same with failed_at and cancelled_at. Nothing indexes those
-- columns, so each such query scanned all of `batches`, and on a large table
-- a scan that runs every minute is slow enough to be cancelled on a hot
-- standby by recovery conflicts.
--
-- One partial index covers all three terminal states: each query's
-- `<state>_at > expires_at` implies the OR predicate, so the planner scans
-- only batches that ever finished late, a tiny fraction of the table.
--
-- One statement only: CONCURRENTLY cannot run in a transaction. The following
-- migrations reindex (repairing an interrupted build) and validate it.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_batches_finished_late
ON batches (id)
WHERE deleted_at IS NULL
  AND (completed_at > expires_at
    OR failed_at > expires_at
    OR cancelled_at > expires_at);
