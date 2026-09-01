-- Restore flex-specific partial indexes.
CREATE INDEX IF NOT EXISTS idx_requests_active_first_flex ON requests
  USING btree (
    (CASE state WHEN 'processing' THEN 0 WHEN 'claimed' THEN 1 WHEN 'pending' THEN 2 ELSE 3 END),
    created_at DESC, id DESC
  ) WHERE service_tier = 'flex' AND batch_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_requests_flex_created ON requests
  USING btree (created_at DESC, id DESC)
  WHERE service_tier = 'flex' AND batch_id IS NOT NULL;

-- Drop composite indexes.
DROP INDEX IF EXISTS idx_requests_created_tier;
DROP INDEX IF EXISTS idx_requests_active_first_tier;
