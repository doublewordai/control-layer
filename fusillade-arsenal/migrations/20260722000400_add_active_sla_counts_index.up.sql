-- no-transaction
-- Background capacity is derived from active SLA work, excluding priority and
-- background rows from that count.
-- run_migrations repairs interrupted builds before SQLx retries this migration.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_requests_active_sla_counts
ON requests (batch_id, model)
WHERE state IN ('pending', 'claimed', 'processing')
  AND template_id IS NOT NULL
  AND (
      service_tier IS NULL
      OR service_tier NOT IN ('priority', 'background')
  );
