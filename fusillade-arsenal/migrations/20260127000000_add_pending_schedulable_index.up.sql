-- Add index optimized for finding schedulable pending requests
-- This supports the query that finds distinct models with pending schedulable requests
-- for non-cancelled batches:
--
--   SELECT DISTINCT r.model FROM requests r
--   JOIN batches b ON r.batch_id = b.id
--   WHERE r.state = 'pending'
--     AND (r.not_before IS NULL OR r.not_before <= $1)
--     AND b.cancelling_at IS NULL
--
-- The index uses NULLS FIRST so that both NULL and small not_before values
-- are at the start of the index, making the OR condition efficient.
-- The INCLUDE (model) allows index-only scans for the DISTINCT projection.

CREATE INDEX idx_requests_pending_batch_not_before_model
ON requests(batch_id, not_before NULLS FIRST)
INCLUDE (model)
WHERE state = 'pending';

COMMENT ON INDEX idx_requests_pending_batch_not_before_model IS
    'Optimized for finding schedulable pending requests: supports batch JOIN, not_before filter, and model projection without heap access';
