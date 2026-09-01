-- Revert: Restore ON DELETE CASCADE on requests.template_id and make it NOT NULL

-- Drop the SET NULL foreign key
ALTER TABLE requests DROP CONSTRAINT requests_template_id_fkey;

-- Make template_id NOT NULL again (this will fail if there are any NULL values)
ALTER TABLE requests ALTER COLUMN template_id SET NOT NULL;

-- Re-add the foreign key with ON DELETE CASCADE
ALTER TABLE requests ADD CONSTRAINT requests_template_id_fkey
    FOREIGN KEY (template_id) REFERENCES request_templates(id) ON DELETE CASCADE;
