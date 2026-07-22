-- Batch Write-Ahead Log: Append-only table for state transition deltas
-- Lightweight trigger writes deltas to WAL on every state change
-- Queries aggregate WAL deltas and compact them periodically

-- 1. Drop the expensive real-time trigger
DROP TRIGGER IF EXISTS update_batch_status_on_request_change ON requests;
DROP FUNCTION IF EXISTS update_batch_on_request_change();

-- 2. Create append-only WAL table with delta counters
-- Each row represents the delta to apply to batch counters from a state transition
CREATE TABLE batch_state_events (
    id BIGSERIAL PRIMARY KEY,
    batch_id UUID NOT NULL REFERENCES batches(id) ON DELETE CASCADE,
    pending_delta BIGINT NOT NULL DEFAULT 0,
    in_progress_delta BIGINT NOT NULL DEFAULT 0,
    completed_delta BIGINT NOT NULL DEFAULT 0,
    failed_delta BIGINT NOT NULL DEFAULT 0,
    canceled_delta BIGINT NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- 3. Index for efficient aggregation by batch_id (critical for query performance)
CREATE INDEX idx_batch_state_events_batch_id ON batch_state_events(batch_id);

-- 4. Index for efficient cleanup of old events
CREATE INDEX idx_batch_state_events_created_at ON batch_state_events(created_at);

-- 5. Lightweight trigger to append state transitions to WAL
-- This is much faster than updating batch counters because:
-- - No row locks on batches table
-- - Append-only writes are fast
-- - No contention between concurrent request updates
CREATE OR REPLACE FUNCTION log_batch_state_change()
RETURNS TRIGGER AS $$
DECLARE
    old_pending BIGINT;
    old_in_progress BIGINT;
    old_completed BIGINT;
    old_failed BIGINT;
    old_canceled BIGINT;
    new_pending BIGINT;
    new_in_progress BIGINT;
    new_completed BIGINT;
    new_failed BIGINT;
    new_canceled BIGINT;
BEGIN
    -- Only process UPDATE operations where state actually changed
    IF TG_OP = 'UPDATE' AND OLD.state IS DISTINCT FROM NEW.state THEN
        -- Map old state to counters
        CASE OLD.state
            WHEN 'pending' THEN
                old_pending := 1; old_in_progress := 0; old_completed := 0; old_failed := 0; old_canceled := 0;
            WHEN 'claimed' THEN
                old_pending := 0; old_in_progress := 1; old_completed := 0; old_failed := 0; old_canceled := 0;
            WHEN 'processing' THEN
                old_pending := 0; old_in_progress := 1; old_completed := 0; old_failed := 0; old_canceled := 0;
            WHEN 'completed' THEN
                old_pending := 0; old_in_progress := 0; old_completed := 1; old_failed := 0; old_canceled := 0;
            WHEN 'failed' THEN
                old_pending := 0; old_in_progress := 0; old_completed := 0; old_failed := 1; old_canceled := 0;
            WHEN 'canceled' THEN
                old_pending := 0; old_in_progress := 0; old_completed := 0; old_failed := 0; old_canceled := 1;
            ELSE
                old_pending := 0; old_in_progress := 0; old_completed := 0; old_failed := 0; old_canceled := 0;
        END CASE;

        -- Map new state to counters
        CASE NEW.state
            WHEN 'pending' THEN
                new_pending := 1; new_in_progress := 0; new_completed := 0; new_failed := 0; new_canceled := 0;
            WHEN 'claimed' THEN
                new_pending := 0; new_in_progress := 1; new_completed := 0; new_failed := 0; new_canceled := 0;
            WHEN 'processing' THEN
                new_pending := 0; new_in_progress := 1; new_completed := 0; new_failed := 0; new_canceled := 0;
            WHEN 'completed' THEN
                new_pending := 0; new_in_progress := 0; new_completed := 1; new_failed := 0; new_canceled := 0;
            WHEN 'failed' THEN
                new_pending := 0; new_in_progress := 0; new_completed := 0; new_failed := 1; new_canceled := 0;
            WHEN 'canceled' THEN
                new_pending := 0; new_in_progress := 0; new_completed := 0; new_failed := 0; new_canceled := 1;
            ELSE
                new_pending := 0; new_in_progress := 0; new_completed := 0; new_failed := 0; new_canceled := 0;
        END CASE;

        -- Insert delta event (append-only, no locks on batches table)
        INSERT INTO batch_state_events (batch_id, pending_delta, in_progress_delta, completed_delta, failed_delta, canceled_delta)
        VALUES (
            NEW.batch_id,
            new_pending - old_pending,
            new_in_progress - old_in_progress,
            new_completed - old_completed,
            new_failed - old_failed,
            new_canceled - old_canceled
        );
    END IF;

    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

-- 6. Create the lightweight WAL-writing trigger
CREATE TRIGGER log_batch_state_change_to_wal
    AFTER UPDATE ON requests
    FOR EACH ROW
    WHEN (OLD.state IS DISTINCT FROM NEW.state)
    EXECUTE FUNCTION log_batch_state_change();

-- 7. Helper function to aggregate WAL events and compact them into batch counters
-- Called by application code when querying batches
CREATE OR REPLACE FUNCTION compact_batch_state_events(p_batch_id UUID)
RETURNS VOID AS $$
BEGIN
    -- Aggregate all deltas for this batch and apply them atomically
    UPDATE batches b
    SET
        pending_requests = b.pending_requests + COALESCE(deltas.pending_sum, 0),
        in_progress_requests = b.in_progress_requests + COALESCE(deltas.in_progress_sum, 0),
        completed_requests = b.completed_requests + COALESCE(deltas.completed_sum, 0),
        failed_requests = b.failed_requests + COALESCE(deltas.failed_sum, 0),
        canceled_requests = b.canceled_requests + COALESCE(deltas.canceled_sum, 0),
        requests_last_updated_at = NOW()
    FROM (
        SELECT
            SUM(pending_delta) as pending_sum,
            SUM(in_progress_delta) as in_progress_sum,
            SUM(completed_delta) as completed_sum,
            SUM(failed_delta) as failed_sum,
            SUM(canceled_delta) as canceled_sum
        FROM batch_state_events
        WHERE batch_id = p_batch_id
    ) deltas
    WHERE b.id = p_batch_id;

    -- Delete processed events for this batch
    DELETE FROM batch_state_events WHERE batch_id = p_batch_id;
END;
$$ LANGUAGE plpgsql;
