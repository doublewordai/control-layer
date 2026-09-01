-- Add status tracking columns directly to batches table with trigger-based updates
-- This provides real-time accuracy without needing a separate table or expensive view refreshes

-- 1. Drop the existing batch_status view
DROP VIEW IF EXISTS batch_status;

-- 2. Add status columns to batches table
ALTER TABLE batches
ADD COLUMN total_requests BIGINT NOT NULL DEFAULT 0,
ADD COLUMN pending_requests BIGINT NOT NULL DEFAULT 0,
ADD COLUMN in_progress_requests BIGINT NOT NULL DEFAULT 0,
ADD COLUMN completed_requests BIGINT NOT NULL DEFAULT 0,
ADD COLUMN failed_requests BIGINT NOT NULL DEFAULT 0,
ADD COLUMN canceled_requests BIGINT NOT NULL DEFAULT 0,
ADD COLUMN requests_started_at TIMESTAMPTZ,
ADD COLUMN requests_last_updated_at TIMESTAMPTZ;

-- 3. Populate initial data from existing requests
UPDATE batches b
SET
    total_requests = COALESCE(counts.total, 0),
    pending_requests = COALESCE(counts.pending, 0),
    in_progress_requests = COALESCE(counts.in_progress, 0),
    completed_requests = COALESCE(counts.completed, 0),
    failed_requests = COALESCE(counts.failed, 0),
    canceled_requests = COALESCE(counts.canceled, 0),
    requests_started_at = counts.started_at,
    requests_last_updated_at = counts.last_updated_at
FROM (
    SELECT
        batch_id,
        COUNT(*) as total,
        COUNT(*) FILTER (WHERE state = 'pending') as pending,
        COUNT(*) FILTER (WHERE state IN ('claimed', 'processing')) as in_progress,
        COUNT(*) FILTER (WHERE state = 'completed') as completed,
        COUNT(*) FILTER (WHERE state = 'failed') as failed,
        COUNT(*) FILTER (WHERE state = 'canceled') as canceled,
        MIN(created_at) as started_at,
        MAX(updated_at) as last_updated_at
    FROM requests
    GROUP BY batch_id
) counts
WHERE b.id = counts.batch_id;

-- 4. Function to update batch status when request state changes
CREATE OR REPLACE FUNCTION update_batch_on_request_change()
RETURNS TRIGGER AS $$
DECLARE
    old_state TEXT;
    new_state TEXT;
BEGIN
    -- Handle INSERT
    IF TG_OP = 'INSERT' THEN
        UPDATE batches
        SET total_requests = total_requests + 1,
            pending_requests = pending_requests + CASE WHEN NEW.state = 'pending' THEN 1 ELSE 0 END,
            in_progress_requests = in_progress_requests + CASE WHEN NEW.state IN ('claimed', 'processing') THEN 1 ELSE 0 END,
            completed_requests = completed_requests + CASE WHEN NEW.state = 'completed' THEN 1 ELSE 0 END,
            failed_requests = failed_requests + CASE WHEN NEW.state = 'failed' THEN 1 ELSE 0 END,
            canceled_requests = canceled_requests + CASE WHEN NEW.state = 'canceled' THEN 1 ELSE 0 END,
            requests_started_at = CASE WHEN requests_started_at IS NULL THEN NEW.created_at ELSE requests_started_at END,
            requests_last_updated_at = NEW.updated_at
        WHERE id = NEW.batch_id;
        RETURN NEW;
    END IF;

    -- Handle UPDATE (state transition)
    IF TG_OP = 'UPDATE' AND OLD.state != NEW.state THEN
        old_state := OLD.state;
        new_state := NEW.state;

        UPDATE batches
        SET
            -- Combined decrement/increment for each state
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

    -- Handle DELETE
    IF TG_OP = 'DELETE' THEN
        UPDATE batches
        SET total_requests = total_requests - 1,
            pending_requests = pending_requests - CASE WHEN OLD.state = 'pending' THEN 1 ELSE 0 END,
            in_progress_requests = in_progress_requests - CASE WHEN OLD.state IN ('claimed', 'processing') THEN 1 ELSE 0 END,
            completed_requests = completed_requests - CASE WHEN OLD.state = 'completed' THEN 1 ELSE 0 END,
            failed_requests = failed_requests - CASE WHEN OLD.state = 'failed' THEN 1 ELSE 0 END,
            canceled_requests = canceled_requests - CASE WHEN OLD.state = 'canceled' THEN 1 ELSE 0 END,
            requests_last_updated_at = NOW()
        WHERE id = OLD.batch_id;
        RETURN OLD;
    END IF;

    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

-- 5. Create trigger on requests table
CREATE TRIGGER update_batch_status_on_request_change
    AFTER INSERT OR UPDATE OR DELETE ON requests
    FOR EACH ROW
    EXECUTE FUNCTION update_batch_on_request_change();
