-- Remove escalation support - replace with route-at-claim-time approach
-- This migration removes the SLA racing/supersession system in favor of
-- routing requests to escalated models at claim time based on time remaining.
--
-- Note: Any escalated/superseded requests will have their tracking columns dropped.
-- If cleanup is needed, run manually before this migration:
--   DELETE FROM requests WHERE is_escalated = true OR state = 'superseded';

-- Drop the views that depend on the columns we're removing
DROP VIEW IF EXISTS active_requests;

-- Drop check constraints
ALTER TABLE requests DROP CONSTRAINT IF EXISTS only_escalated_has_parent;
ALTER TABLE requests DROP CONSTRAINT IF EXISTS escalated_must_have_original;

-- Drop indexes that reference the columns we're removing
DROP INDEX IF EXISTS idx_requests_model_escalated_state;
DROP INDEX IF EXISTS idx_requests_escalated_from_state;

-- Drop foreign key constraints on escalation columns
ALTER TABLE requests DROP CONSTRAINT IF EXISTS requests_escalated_from_request_id_fkey;
ALTER TABLE requests DROP CONSTRAINT IF EXISTS requests_superseded_by_request_id_fkey;

-- Now drop the columns
ALTER TABLE requests
    DROP COLUMN IF EXISTS superseded_by_request_id,
    DROP COLUMN IF EXISTS superseded_at,
    DROP COLUMN IF EXISTS is_escalated,
    DROP COLUMN IF EXISTS escalated_from_request_id;

-- Revert state_check constraint to original states (remove 'superseded')
ALTER TABLE requests DROP CONSTRAINT state_check;
ALTER TABLE requests ADD CONSTRAINT state_check CHECK (
    state IN ('pending', 'claimed', 'processing', 'completed', 'failed', 'canceled')
);

-- Add routed_model to track what model was actually used (may differ from template
-- if request was routed to escalation model at claim time)
ALTER TABLE requests ADD COLUMN routed_model TEXT NULL;

-- Recreate the active_requests view without the removed columns, including routed_model
-- Preserves soft delete filtering from 20260129000001_update_active_views
CREATE VIEW active_requests AS
SELECT r.*
FROM requests r
JOIN batches b ON r.batch_id = b.id
WHERE b.deleted_at IS NULL;
