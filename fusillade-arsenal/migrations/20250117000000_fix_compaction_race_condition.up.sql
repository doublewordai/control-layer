-- Fix race condition in compact_batch_state_events function
-- The original implementation had a race where concurrent compactions could double-apply deltas

CREATE OR REPLACE FUNCTION compact_batch_state_events(p_batch_id UUID)
RETURNS VOID AS $$
DECLARE
    pending_sum BIGINT;
    in_progress_sum BIGINT;
    completed_sum BIGINT;
    failed_sum BIGINT;
    canceled_sum BIGINT;
BEGIN
    -- Atomically claim and delete events using DELETE ... RETURNING
    -- This prevents concurrent compactions from processing the same events
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

    -- Only update if there were events to compact
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
