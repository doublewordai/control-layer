-- no-transaction
-- Steady sweep and backfill candidate discovery; verified by
-- retained_response_archive_index_ready(). Several minutes on a
-- production-sized requests table: deploy in a quiet window.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_requests_batchless_retention_due
    ON requests (
        service_tier,
        (CASE state WHEN 'completed' THEN completed_at
                    WHEN 'failed' THEN failed_at
                    WHEN 'canceled' THEN canceled_at END),
        id
    )
    WHERE batch_id IS NULL
      AND state IN ('completed', 'failed', 'canceled');
