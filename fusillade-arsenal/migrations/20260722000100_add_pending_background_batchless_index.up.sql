-- no-transaction
-- Background batchless requests have no batch parent and are claimed by model.
-- run_migrations removes an invalid interrupted build before retry; IF NOT
-- EXISTS preserves a valid build if only SQLx bookkeeping was interrupted.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_requests_pending_background_batchless
ON requests (model, created_at, id)
WHERE state = 'pending'
  AND batch_id IS NULL
  AND template_id IS NOT NULL
  AND service_tier = 'background';
