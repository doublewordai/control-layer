-- Rollback: Restore WAL system
-- This is complex and requires recreating all the infrastructure

-- 1. Restore counter columns
ALTER TABLE batches
    ADD COLUMN IF NOT EXISTS pending_requests BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS in_progress_requests BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS completed_requests BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS failed_requests BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS canceled_requests BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS requests_last_updated_at TIMESTAMPTZ;

-- 2. Recalculate counters from actual request states
UPDATE batches b
SET
    pending_requests = actual_counts.pending,
    in_progress_requests = actual_counts.in_progress,
    completed_requests = actual_counts.completed,
    failed_requests = actual_counts.failed,
    canceled_requests = actual_counts.canceled,
    requests_last_updated_at = NOW()
FROM (
    SELECT
        batch_id,
        COUNT(*) FILTER (WHERE state = 'pending') as pending,
        COUNT(*) FILTER (WHERE state IN ('claimed', 'processing')) as in_progress,
        COUNT(*) FILTER (WHERE state = 'completed') as completed,
        COUNT(*) FILTER (WHERE state = 'failed') as failed,
        COUNT(*) FILTER (WHERE state = 'canceled') as canceled
    FROM requests
    GROUP BY batch_id
) actual_counts
WHERE b.id = actual_counts.batch_id;

-- 3. Recreate batch_state_events table
CREATE TABLE IF NOT EXISTS batch_state_events (
    id BIGSERIAL PRIMARY KEY,
    batch_id UUID NOT NULL REFERENCES batches(id) ON DELETE CASCADE,
    pending_delta BIGINT NOT NULL DEFAULT 0,
    in_progress_delta BIGINT NOT NULL DEFAULT 0,
    completed_delta BIGINT NOT NULL DEFAULT 0,
    failed_delta BIGINT NOT NULL DEFAULT 0,
    canceled_delta BIGINT NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_batch_state_events_batch_id ON batch_state_events(batch_id);
CREATE INDEX IF NOT EXISTS idx_batch_state_events_created_at ON batch_state_events(created_at);

-- 4. Recreate compaction function
CREATE OR REPLACE FUNCTION compact_batch_state_events(p_batch_id UUID)
RETURNS VOID AS $$
DECLARE
    pending_sum BIGINT;
    in_progress_sum BIGINT;
    completed_sum BIGINT;
    failed_sum BIGINT;
    canceled_sum BIGINT;
BEGIN
    WITH deleted_events AS (
        DELETE FROM batch_state_events
        WHERE batch_id = p_batch_id
        RETURNING pending_delta, in_progress_delta, completed_delta, failed_delta, canceled_delta
    ),
    sums AS (
        SELECT
            COALESCE(SUM(pending_delta), 0) as pending_sum,
            COALESCE(SUM(in_progress_delta), 0) as in_progress_sum,
            COALESCE(SUM(completed_delta), 0) as completed_sum,
            COALESCE(SUM(failed_delta), 0) as failed_sum,
            COALESCE(SUM(canceled_delta), 0) as canceled_sum
        FROM deleted_events
    )
    SELECT * INTO pending_sum, in_progress_sum, completed_sum, failed_sum, canceled_sum FROM sums;

    IF pending_sum != 0 OR in_progress_sum != 0 OR completed_sum != 0 OR failed_sum != 0 OR canceled_sum != 0 THEN
        UPDATE batches
        SET
            pending_requests = pending_requests + pending_sum,
            in_progress_requests = in_progress_requests + in_progress_sum,
            completed_requests = completed_requests + completed_sum,
            failed_requests = failed_requests + failed_sum,
            canceled_requests = canceled_requests + canceled_sum,
            requests_last_updated_at = NOW()
        WHERE id = p_batch_id;
    END IF;
END;
$$ LANGUAGE plpgsql;

-- 5. Recreate trigger function
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
    IF TG_OP = 'UPDATE' AND OLD.state IS DISTINCT FROM NEW.state THEN
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

-- 6. Recreate trigger
CREATE TRIGGER log_batch_state_change_to_wal
    AFTER UPDATE ON requests
    FOR EACH ROW
    WHEN (OLD.state IS DISTINCT FROM NEW.state)
    EXECUTE FUNCTION log_batch_state_change();

-- 7. Drop the composite index (since we're back to cached counters)
DROP INDEX IF EXISTS idx_requests_batch_state;
