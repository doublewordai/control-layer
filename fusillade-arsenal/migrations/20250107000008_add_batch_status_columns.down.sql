-- Revert batch status columns back to view-based approach

-- 1. Drop the trigger and function
DROP TRIGGER IF EXISTS update_batch_status_on_request_change ON requests;
DROP FUNCTION IF EXISTS update_batch_on_request_change();

-- 2. Drop the status columns from batches table
ALTER TABLE batches
DROP COLUMN IF EXISTS total_requests,
DROP COLUMN IF EXISTS pending_requests,
DROP COLUMN IF EXISTS in_progress_requests,
DROP COLUMN IF EXISTS completed_requests,
DROP COLUMN IF EXISTS failed_requests,
DROP COLUMN IF EXISTS canceled_requests,
DROP COLUMN IF EXISTS requests_started_at,
DROP COLUMN IF EXISTS requests_last_updated_at;

-- 3. Recreate the original batch_status VIEW
CREATE VIEW batch_status AS
SELECT
    b.id as batch_id,
    b.file_id,
    f.name as file_name,
    COUNT(*) as total_requests,
    COUNT(*) FILTER (WHERE r.state = 'pending') as pending_requests,
    COUNT(*) FILTER (WHERE r.state IN ('claimed', 'processing')) as in_progress_requests,
    COUNT(*) FILTER (WHERE r.state = 'completed') as completed_requests,
    COUNT(*) FILTER (WHERE r.state = 'failed') as failed_requests,
    COUNT(*) FILTER (WHERE r.state = 'canceled') as canceled_requests,
    MIN(r.created_at) as started_at,
    MAX(r.updated_at) as last_updated_at,
    b.created_at
FROM batches b
JOIN files f ON f.id = b.file_id
LEFT JOIN requests r ON r.batch_id = b.id
GROUP BY b.id, b.file_id, f.name, b.created_at;
