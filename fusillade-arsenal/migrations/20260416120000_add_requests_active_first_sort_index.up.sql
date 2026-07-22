-- Expression index supporting list_requests with active_first=true.
--
-- list_requests orders by
--   CASE state WHEN 'processing' THEN 0 WHEN 'claimed' THEN 1 WHEN 'pending' THEN 2 ELSE 3 END ASC,
--   created_at DESC, id DESC
-- Without an index on that exact expression, the planner has to materialize
-- the entire filtered JOIN result and sort it, which times out on large data.
--
-- Mirrors the idx_batches_active_first_sort pattern already used on `batches`.
-- Partial on batch_id IS NOT NULL to skip requests orphaned by FK cascade.
--
-- NOTE: CREATE INDEX without CONCURRENTLY takes an ACCESS EXCLUSIVE lock for
-- the duration of the build. On large tables create this CONCURRENTLY ahead
-- of applying the migration; the IF NOT EXISTS below makes the migration a
-- no-op in that case:
--
--   CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_requests_active_first_sort
--   ON requests (
--     (CASE state WHEN 'processing' THEN 0 WHEN 'claimed' THEN 1 WHEN 'pending' THEN 2 ELSE 3 END),
--     created_at DESC,
--     id DESC
--   ) WHERE batch_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_requests_active_first_sort
ON requests (
  (CASE state WHEN 'processing' THEN 0 WHEN 'claimed' THEN 1 WHEN 'pending' THEN 2 ELSE 3 END),
  created_at DESC,
  id DESC
) WHERE batch_id IS NOT NULL;
