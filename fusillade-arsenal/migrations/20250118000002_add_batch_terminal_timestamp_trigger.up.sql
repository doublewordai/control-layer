-- Create trigger to automatically set batch terminal state timestamps
-- This trigger fires when a request transitions to a terminal state (completed, failed, canceled)
-- and checks if the batch has now entered a terminal status

CREATE OR REPLACE FUNCTION update_batch_terminal_timestamps()
RETURNS TRIGGER AS $$
DECLARE
    batch_record RECORD;
    total_count BIGINT;
    terminal_count BIGINT;
    completed_count BIGINT;
    failed_count BIGINT;
    canceled_count BIGINT;
    terminal_ratio NUMERIC;
BEGIN
    -- Only process if transitioning TO a terminal state
    IF NEW.state NOT IN ('completed', 'failed', 'canceled') THEN
        RETURN NEW;
    END IF;

    -- Get current counts for this batch
    SELECT
        COUNT(*) as total,
        COUNT(*) FILTER (WHERE state IN ('completed', 'failed', 'canceled')) as terminal,
        COUNT(*) FILTER (WHERE state = 'completed') as completed,
        COUNT(*) FILTER (WHERE state = 'failed') as failed,
        COUNT(*) FILTER (WHERE state = 'canceled') as canceled
    INTO total_count, terminal_count, completed_count, failed_count, canceled_count
    FROM requests
    WHERE batch_id = NEW.batch_id;

    -- Calculate terminal ratio for finalizing check
    IF total_count > 0 THEN
        terminal_ratio := terminal_count::NUMERIC / total_count::NUMERIC;
    ELSE
        terminal_ratio := 0;
    END IF;

    -- Set finalizing_at when 95% or more requests are terminal (if not already set)
    IF terminal_ratio >= 0.95 THEN
        UPDATE batches
        SET finalizing_at = COALESCE(finalizing_at, NOW())
        WHERE id = NEW.batch_id
          AND finalizing_at IS NULL;
    END IF;

    -- Set terminal timestamps only when ALL requests are in terminal states
    IF terminal_count = total_count AND total_count > 0 THEN
        -- Determine which terminal timestamp to set based on the batch outcome
        IF canceled_count = total_count THEN
            -- All requests canceled -> set cancelled_at
            UPDATE batches
            SET cancelled_at = COALESCE(cancelled_at, NOW())
            WHERE id = NEW.batch_id
              AND cancelled_at IS NULL;
        ELSIF completed_count = 0 THEN
            -- No successful completions and at least one request -> failed
            UPDATE batches
            SET failed_at = COALESCE(failed_at, NOW())
            WHERE id = NEW.batch_id
              AND failed_at IS NULL;
        ELSE
            -- At least one completion -> completed
            UPDATE batches
            SET completed_at = COALESCE(completed_at, NOW())
            WHERE id = NEW.batch_id
              AND completed_at IS NULL;
        END IF;
    END IF;

    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

-- Create trigger that fires AFTER each request state update
CREATE TRIGGER update_batch_terminal_timestamps_trigger
    AFTER UPDATE OF state ON requests
    FOR EACH ROW
    WHEN (NEW.state IS DISTINCT FROM OLD.state)
    EXECUTE FUNCTION update_batch_terminal_timestamps();

-- Add comment explaining the trigger
COMMENT ON FUNCTION update_batch_terminal_timestamps() IS
    'Automatically updates batch terminal timestamps (finalizing_at, completed_at, failed_at, cancelled_at) when requests reach terminal states';
