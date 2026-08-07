-- Indexes for get_completed_request_counts_by_model_and_window (trailing
-- demand windows on /monitoring/demand).
--
-- The trailing branch counts terminal rows whose outcome timestamp falls in
-- a recent window (typically the last hour), one column per terminal state:
-- completed_at for 'completed', failed_at for 'failed'. Without these, the
-- only completed_at index is the flex-decay partial
-- (idx_requests_completed_flex_decay), which excludes exactly the priority
-- rows the trailing windows exist to count, and failed rows have no
-- timestamp index at all.
--
-- The predicates can't pre-filter by time (NOW() isn't immutable), so each
-- index covers all rows of its state; queries only ever descend the recent
-- tail. INCLUDE columns are best-effort: recent heap pages aren't
-- all-visible yet, so scans mostly heap-fetch anyway.
--
-- On large production tables, create these CONCURRENTLY before deploying
-- this migration so the IF NOT EXISTS statements become no-ops:
--
--   CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_requests_completed_trailing
--   ON requests (completed_at DESC) INCLUDE (model, service_tier)
--   WHERE state = 'completed';
--
--   CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_requests_failed_trailing
--   ON requests (failed_at DESC) INCLUDE (model, service_tier)
--   WHERE state = 'failed';

CREATE INDEX IF NOT EXISTS idx_requests_completed_trailing
ON requests (completed_at DESC) INCLUDE (model, service_tier)
WHERE state = 'completed';

CREATE INDEX IF NOT EXISTS idx_requests_failed_trailing
ON requests (failed_at DESC) INCLUDE (model, service_tier)
WHERE state = 'failed';
