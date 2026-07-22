-- Add escalation support for SLA race-based priority endpoint routing
-- This migration adds columns and indexes to track request escalation pairs
-- and race completion (supersession) handling.

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
-- Supports: WHERE esc.escalated_from_request_id = r.id AND esc.state IN (...)
CREATE INDEX idx_requests_escalated_from_state
ON requests(escalated_from_request_id, state)
WHERE escalated_from_request_id IS NOT NULL;

-- Composite index for create_escalated_requests main query
-- Supports: WHERE r.model = $1 AND r.is_escalated = false AND r.state = ANY($2)
CREATE INDEX idx_requests_model_escalated_state
ON requests(model, is_escalated, state)
WHERE is_escalated = false AND state IN ('pending', 'claimed', 'processing');

-- Add constraint: escalated requests must reference an original request
-- Prevents orphaned escalated requests with is_escalated=true but no link
ALTER TABLE requests
    ADD CONSTRAINT escalated_must_have_original
    CHECK ((is_escalated = false) OR (escalated_from_request_id IS NOT NULL));

-- Add constraint: only escalated requests can have escalated_from_request_id
-- Prevents original requests from accidentally being marked as escalations
ALTER TABLE requests
    ADD CONSTRAINT only_escalated_has_parent
    CHECK ((escalated_from_request_id IS NULL) OR (is_escalated = true));

-- Documentation comments
COMMENT ON COLUMN requests.escalated_from_request_id IS
    'If this is an escalated request (is_escalated=true), references the original request that was escalated';

COMMENT ON COLUMN requests.is_escalated IS
    'True if this request was created as an SLA escalation to a priority endpoint. Escalated requests are infrastructure and not counted in batch progress.';

COMMENT ON COLUMN requests.superseded_at IS
    'When this request was superseded by its racing pair completing first. Superseded requests are terminal and do not count toward batch completion.';

COMMENT ON COLUMN requests.superseded_by_request_id IS
    'Which request (original or escalated) completed first and superseded this one in the race';
