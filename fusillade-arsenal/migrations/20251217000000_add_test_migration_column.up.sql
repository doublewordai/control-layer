-- Test migration: Add a dummy column to requests table
-- This migration is backwards compatible - adds a nullable column with a default

ALTER TABLE requests
ADD COLUMN test_migration_column TEXT NULL;

-- Add comment to explain this is for testing
COMMENT ON COLUMN requests.test_migration_column IS 
    'Test column for validating deployment consistency during schema migrations. Can be safely removed.';

-- Add a default value for new rows (backwards compatible)
ALTER TABLE requests
ALTER COLUMN test_migration_column SET DEFAULT 'migration_test_value';

-- Create a partial index (optional, but makes it more realistic)
CREATE INDEX idx_requests_test_migration 
ON requests(test_migration_column)
WHERE test_migration_column IS NOT NULL;