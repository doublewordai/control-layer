-- Add a broader partial index for the capacity-check query that counts active
-- requests grouped by model and completion window during batch creation.
--
-- The query filters on r.state = ANY('{pending,claimed,processing}') but the
-- existing idx_requests_pending only matches state = 'pending', so Postgres
-- cannot use it and falls back to a sequential scan on the full requests table.
--
-- This new index covers all three active states. The existing
-- idx_requests_pending is kept because the batch daemon's claim query filters
-- on state = 'pending' alone and benefits from the smaller, tighter index.
--
-- NOTE: Create the index on production FIRST using CONCURRENTLY before
-- deploying this migration:
--
--   CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_requests_active_with_template
--   ON requests (model, batch_id)
--   WHERE state IN ('pending', 'claimed', 'processing')
--     AND template_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_requests_active_with_template
ON requests (model, batch_id)
WHERE state IN ('pending', 'claimed', 'processing')
  AND template_id IS NOT NULL;
