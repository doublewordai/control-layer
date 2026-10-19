DROP INDEX IF EXISTS idx_requests_active_batched_demand;

CREATE INDEX IF NOT EXISTS idx_requests_active_non_priority_counts
ON requests (batch_id, model)
WHERE state IN ('pending', 'claimed', 'processing')
  AND template_id IS NOT NULL
  AND (service_tier IS NULL OR service_tier <> 'priority');

CREATE INDEX IF NOT EXISTS idx_requests_active_sla_counts
ON requests (batch_id, model)
WHERE state IN ('pending', 'claimed', 'processing')
  AND template_id IS NOT NULL
  AND (
      service_tier IS NULL
      OR service_tier NOT IN ('priority', 'background')
  );
