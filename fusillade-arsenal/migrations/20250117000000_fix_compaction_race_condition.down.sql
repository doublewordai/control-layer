-- Revert to original (buggy) compaction function

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
