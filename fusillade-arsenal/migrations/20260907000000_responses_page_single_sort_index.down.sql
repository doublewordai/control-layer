-- Restore the two rank-ordered listing indexes and drop the archive sort index.
--
-- Definitions transcribed from 20260630000000_formalize_user_sort_indexes and
-- from production pg_get_indexdef for idx_requests_active_first_tier.
--
-- Both rebuilds are non-concurrent and will hold ACCESS EXCLUSIVE on `requests`
-- for the duration; each is several GB on a production-sized database. Build
-- them CONCURRENTLY out-of-band first if this ever has to be run against a live
-- deployment.

CREATE INDEX IF NOT EXISTS idx_requests_user_active_sort
  ON requests (
    created_by,
    (CASE state WHEN 'processing' THEN 0 WHEN 'claimed' THEN 1 WHEN 'pending' THEN 2 ELSE 3 END),
    created_at DESC,
    id DESC,
    service_tier
  ) WHERE created_by IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_requests_active_first_tier
  ON requests (
    (CASE state WHEN 'processing' THEN 0 WHEN 'claimed' THEN 1 WHEN 'pending' THEN 2 ELSE 3 END),
    created_at DESC,
    id DESC,
    service_tier
  );

DROP INDEX IF EXISTS idx_retained_response_objects_created;
