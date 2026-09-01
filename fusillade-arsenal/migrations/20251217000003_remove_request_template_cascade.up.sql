-- Remove ON DELETE CASCADE from requests.template_id and make it nullable with SET NULL
-- This prevents requests from being deleted when their template is deleted

-- First, drop the existing foreign key constraint
ALTER TABLE requests DROP CONSTRAINT requests_template_id_fkey;

-- Make template_id nullable
ALTER TABLE requests ALTER COLUMN template_id DROP NOT NULL;

-- Re-add the foreign key with ON DELETE SET NULL
ALTER TABLE requests ADD CONSTRAINT requests_template_id_fkey
    FOREIGN KEY (template_id) REFERENCES request_templates(id) ON DELETE SET NULL;
