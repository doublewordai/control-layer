-- Remove ON DELETE CASCADE from request_templates.file_id and make it nullable with SET NULL
-- This prevents templates from being deleted when their file is deleted, avoiding
-- unbounded cascade deletes for files with many templates.

-- Drop the existing foreign key constraint
ALTER TABLE request_templates DROP CONSTRAINT request_templates_file_id_fkey;

-- Make file_id nullable
ALTER TABLE request_templates ALTER COLUMN file_id DROP NOT NULL;

-- Re-add the foreign key with ON DELETE SET NULL
ALTER TABLE request_templates ADD CONSTRAINT request_templates_file_id_fkey
    FOREIGN KEY (file_id) REFERENCES files(id) ON DELETE SET NULL;

-- Create a view for active (non-orphaned) templates
-- Queries should use this view instead of request_templates directly when joining
CREATE VIEW active_request_templates AS
SELECT * FROM request_templates WHERE file_id IS NOT NULL;
