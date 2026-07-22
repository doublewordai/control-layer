-- Indexes for get_pending_request_counts_by_model_and_window.
--
-- The normal queue-depth branch filters active templated requests, excludes
-- priority tier rows, groups by batch_id/model, and then applies expiry
-- windows. A partial index on exactly those countable active rows, ordered by
-- batch_id/model, lets Postgres perform that pre-aggregation as an index-only
-- grouped scan.
--
-- The decay branch counts recently completed flex rows in the 1h bucket. The
-- existing idx_requests_state index finds all completed rows and then filters
-- by service_tier and completed_at; this partial index makes that branch
-- selective by tier and completion time.
--
-- On large production tables, create these CONCURRENTLY before deploying this
-- migration so the IF NOT EXISTS statements become no-ops:
--
--   CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_requests_active_non_priority_counts
--   ON requests (batch_id, model)
--   WHERE state IN ('pending', 'claimed', 'processing')
--     AND template_id IS NOT NULL
--     AND (service_tier IS NULL OR service_tier <> 'priority');
--
--   CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_requests_completed_flex_decay
--   ON requests (completed_at DESC) INCLUDE (model)
--   WHERE state = 'completed'
--     AND service_tier = 'flex'
--     AND template_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_requests_active_non_priority_counts
ON requests (batch_id, model)
WHERE state IN ('pending', 'claimed', 'processing')
  AND template_id IS NOT NULL
  AND (service_tier IS NULL OR service_tier <> 'priority');

CREATE INDEX IF NOT EXISTS idx_requests_completed_flex_decay
ON requests (completed_at DESC) INCLUDE (model)
WHERE state = 'completed'
  AND service_tier = 'flex'
  AND template_id IS NOT NULL;
