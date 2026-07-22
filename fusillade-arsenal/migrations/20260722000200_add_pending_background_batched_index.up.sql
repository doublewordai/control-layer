-- no-transaction
-- File-backed background requests retain their batch parent, so keep this hot
-- claim path separate from the batchless source.
-- run_migrations repairs interrupted builds before SQLx retries this migration.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_requests_pending_background_batched
ON requests (model, batch_id, created_at, id)
WHERE state = 'pending'
  AND batch_id IS NOT NULL
  AND template_id IS NOT NULL
  AND service_tier = 'background';
