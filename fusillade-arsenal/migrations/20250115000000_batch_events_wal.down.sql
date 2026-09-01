-- Rollback: Restore the real-time trigger approach

DROP TRIGGER IF EXISTS log_batch_state_change_to_wal ON requests;
DROP FUNCTION IF EXISTS log_batch_state_change();
DROP FUNCTION IF EXISTS compact_batch_state_events(UUID);
DROP TABLE IF EXISTS batch_state_events;

-- Restore the real-time trigger function
CREATE OR REPLACE FUNCTION update_batch_on_request_change()
RETURNS TRIGGER AS $$
DECLARE
    old_state TEXT;
    new_state TEXT;
BEGIN
    IF TG_OP = 'UPDATE' AND OLD.state IS DISTINCT FROM NEW.state THEN
        old_state := OLD.state;
        new_state := NEW.state;

        UPDATE batches
        SET
            pending_requests = pending_requests
                - CASE WHEN old_state = 'pending' THEN 1 ELSE 0 END
                + CASE WHEN new_state = 'pending' THEN 1 ELSE 0 END,
            in_progress_requests = in_progress_requests
                - CASE WHEN old_state IN ('claimed', 'processing') THEN 1 ELSE 0 END
                + CASE WHEN new_state IN ('claimed', 'processing') THEN 1 ELSE 0 END,
            completed_requests = completed_requests
                - CASE WHEN old_state = 'completed' THEN 1 ELSE 0 END
                + CASE WHEN new_state = 'completed' THEN 1 ELSE 0 END,
            failed_requests = failed_requests
                - CASE WHEN old_state = 'failed' THEN 1 ELSE 0 END
                + CASE WHEN new_state = 'failed' THEN 1 ELSE 0 END,
            canceled_requests = canceled_requests
                - CASE WHEN old_state = 'canceled' THEN 1 ELSE 0 END
                + CASE WHEN new_state = 'canceled' THEN 1 ELSE 0 END,
            requests_last_updated_at = NEW.updated_at
        WHERE id = NEW.batch_id;
        RETURN NEW;
    END IF;

    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER update_batch_status_on_request_change
    AFTER UPDATE ON requests
    FOR EACH ROW
    WHEN (OLD.state IS DISTINCT FROM NEW.state)
    EXECUTE FUNCTION update_batch_on_request_change();
