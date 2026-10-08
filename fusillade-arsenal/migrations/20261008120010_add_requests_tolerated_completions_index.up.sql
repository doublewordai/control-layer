-- no-transaction
--
-- Throughput of our own workers for the spillover-tolerations release.
--
-- Once per refresh interval one daemon reads, for each model with
-- outstanding work,
--
--   SELECT count(*), sum(completed_at - started_at)
--   FROM requests
--   WHERE dispatched_tolerated AND state = 'completed'
--     AND model = $model AND completed_at >= $window_start
--
-- The partial predicate keeps only successful completions that were
-- dispatched with tolerations, the (model, completed_at) key turns each
-- model's window into one range scan, and started_at is included so the
-- whole read is index-only.
--
-- One statement only: CONCURRENTLY cannot run in a transaction. The following
-- migrations reindex (repairing an interrupted build) and validate it.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_requests_tolerated_completions
ON requests (model, completed_at) INCLUDE (started_at)
WHERE dispatched_tolerated
  AND state = 'completed';
