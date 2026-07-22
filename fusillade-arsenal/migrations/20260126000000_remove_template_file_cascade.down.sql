-- Revert: Restore ON DELETE CASCADE on request_templates.file_id and make it NOT NULL

-- Drop the view
DROP VIEW IF EXISTS active_request_templates;

-- Drop the SET NULL foreign key
ALTER TABLE request_templates DROP CONSTRAINT request_templates_file_id_fkey;

-- Make file_id NOT NULL again
ALTER TABLE request_templates ALTER COLUMN file_id SET NOT NULL;

-- Re-add the foreign key with ON DELETE CASCADE
ALTER TABLE request_templates ADD CONSTRAINT request_templates_file_id_fkey
    FOREIGN KEY (file_id) REFERENCES files(id) ON DELETE CASCADE;
