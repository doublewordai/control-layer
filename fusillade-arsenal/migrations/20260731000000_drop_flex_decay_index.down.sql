CREATE INDEX IF NOT EXISTS idx_requests_completed_flex_decay
ON requests (completed_at DESC) INCLUDE (model)
WHERE state = 'completed'
  AND service_tier = 'flex'
  AND template_id IS NOT NULL;
