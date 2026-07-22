-- no-transaction
-- Keep the existing SLA batchless claim from scanning a large background
-- backlog once background submission is enabled.
-- run_migrations repairs interrupted builds before SQLx retries this migration.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_requests_pending_batchless_sla
ON requests (model, created_at, id)
WHERE state = 'pending'
  AND batch_id IS NULL
  AND template_id IS NOT NULL
  AND service_tier IS DISTINCT FROM 'background';
