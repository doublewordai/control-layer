-- Restore escalation support columns, constraints, and indexes
-- Note: This does not restore any deleted escalated/superseded requests

-- Drop routed_model column added in the up migration
ALTER TABLE requests DROP COLUMN IF EXISTS routed_model;

-- Add 'superseded' to the allowed states
ALTER TABLE requests DROP CONSTRAINT state_check;
ALTER TABLE requests ADD CONSTRAINT state_check CHECK (
    state IN ('pending', 'claimed', 'processing', 'completed', 'failed', 'canceled', 'superseded')
);

-- Add escalation tracking columns to requests table
ALTER TABLE requests
    ADD COLUMN escalated_from_request_id UUID REFERENCES requests(id) ON DELETE SET NULL,
    ADD COLUMN is_escalated BOOLEAN NOT NULL DEFAULT false,
    ADD COLUMN superseded_at TIMESTAMPTZ NULL,
    ADD COLUMN superseded_by_request_id UUID REFERENCES requests(id) ON DELETE SET NULL;

-- Index for NOT EXISTS check in create_escalated_requests and supersede_racing_pair
CREATE INDEX idx_requests_escalated_from_state
ON requests(escalated_from_request_id, state)
WHERE escalated_from_request_id IS NOT NULL;

-- Composite index for create_escalated_requests main query
CREATE INDEX idx_requests_model_escalated_state
ON requests(model, is_escalated, state)
WHERE is_escalated = false AND state IN ('pending', 'claimed', 'processing');

-- Add constraint: escalated requests must reference an original request
ALTER TABLE requests
    ADD CONSTRAINT escalated_must_have_original
    CHECK ((is_escalated = false) OR (escalated_from_request_id IS NOT NULL));

-- Add constraint: only escalated requests can have escalated_from_request_id
ALTER TABLE requests
    ADD CONSTRAINT only_escalated_has_parent
    CHECK ((escalated_from_request_id IS NULL) OR (is_escalated = true));
