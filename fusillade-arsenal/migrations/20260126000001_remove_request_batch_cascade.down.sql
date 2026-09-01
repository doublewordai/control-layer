-- Revert: Restore ON DELETE CASCADE on requests.batch_id and make it NOT NULL

-- Drop the view
DROP VIEW IF EXISTS active_requests;

-- Drop the SET NULL foreign key
ALTER TABLE requests DROP CONSTRAINT requests_batch_id_fkey;

-- Make batch_id NOT NULL again
ALTER TABLE requests ALTER COLUMN batch_id SET NOT NULL;

-- Re-add the foreign key with ON DELETE CASCADE
ALTER TABLE requests ADD CONSTRAINT requests_batch_id_fkey
    FOREIGN KEY (batch_id) REFERENCES batches(id) ON DELETE CASCADE;
