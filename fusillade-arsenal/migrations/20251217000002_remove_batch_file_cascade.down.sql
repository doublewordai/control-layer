-- Revert: Restore ON DELETE CASCADE on batches.file_id and make it NOT NULL

-- Drop the SET NULL foreign key
ALTER TABLE batches DROP CONSTRAINT batches_file_id_fkey;

-- Make file_id NOT NULL again (this will fail if there are any NULL values)
ALTER TABLE batches ALTER COLUMN file_id SET NOT NULL;

-- Re-add the foreign key with ON DELETE CASCADE
ALTER TABLE batches ADD CONSTRAINT batches_file_id_fkey
    FOREIGN KEY (file_id) REFERENCES files(id) ON DELETE CASCADE;
