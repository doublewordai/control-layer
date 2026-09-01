-- Restore both indexes to their pre-migration definitions, verbatim from
-- 20260416120000 (active_first_sort) and 20260204000000 (pending_notification).
CREATE INDEX IF NOT EXISTS idx_requests_active_first_sort
ON requests (
  (CASE state WHEN 'processing' THEN 0 WHEN 'claimed' THEN 1 WHEN 'pending' THEN 2 ELSE 3 END),
  created_at DESC,
  id DESC
) WHERE batch_id IS NOT NULL;

DROP INDEX IF EXISTS idx_batches_pending_notification;
CREATE INDEX idx_batches_pending_notification
    ON batches (id)
    WHERE (completed_at IS NOT NULL OR failed_at IS NOT NULL OR cancelled_at IS NOT NULL)
      AND notification_sent_at IS NULL
      AND deleted_at IS NULL;
