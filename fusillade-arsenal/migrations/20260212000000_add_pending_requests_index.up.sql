-- Add partial index for pending requests with a template.
-- Optimises the batch-window count query that groups pending requests
-- by model and completion_window:
--
--   SELECT r.model, ... COUNT(*) ...
--   FROM requests r
--   JOIN batches b ON r.batch_id = b.id
--   WHERE r.state = 'pending'
--     AND r.template_id IS NOT NULL
--     AND b.cancelling_at IS NULL
--   GROUP BY r.model, ...
--
-- The partial filter (state = 'pending' AND template_id IS NOT NULL) keeps
-- the index small and targets exactly the rows this query needs.

CREATE INDEX IF NOT EXISTS idx_requests_pending
ON requests (model, batch_id)
WHERE state = 'pending' AND template_id IS NOT NULL;
