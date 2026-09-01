-- Remove ON DELETE CASCADE from batches.file_id and make it nullable with SET NULL
-- This prevents batches from being deleted when their input file is deleted

-- First, drop the existing foreign key constraint
ALTER TABLE batches DROP CONSTRAINT batches_file_id_fkey;

-- Make file_id nullable
ALTER TABLE batches ALTER COLUMN file_id DROP NOT NULL;

-- Re-add the foreign key with ON DELETE SET NULL
ALTER TABLE batches ADD CONSTRAINT batches_file_id_fkey
    FOREIGN KEY (file_id) REFERENCES files(id) ON DELETE SET NULL;
