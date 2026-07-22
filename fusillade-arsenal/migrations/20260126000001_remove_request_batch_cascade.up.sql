-- Remove ON DELETE CASCADE from requests.batch_id and make it nullable with SET NULL
-- This prevents requests from being deleted when their batch is deleted, avoiding
-- unbounded cascade deletes for batches with many requests.

-- Drop the existing foreign key constraint
ALTER TABLE requests DROP CONSTRAINT requests_batch_id_fkey;

-- Make batch_id nullable
ALTER TABLE requests ALTER COLUMN batch_id DROP NOT NULL;

-- Re-add the foreign key with ON DELETE SET NULL
ALTER TABLE requests ADD CONSTRAINT requests_batch_id_fkey
    FOREIGN KEY (batch_id) REFERENCES batches(id) ON DELETE SET NULL;

-- Create a view for active (non-orphaned) requests
-- Queries should use this view instead of requests directly when needed
CREATE VIEW active_requests AS
SELECT * FROM requests WHERE batch_id IS NOT NULL;
