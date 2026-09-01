-- Restore test migration column (reverse of removal)

ALTER TABLE requests
ADD COLUMN test_migration_column TEXT NULL DEFAULT 'migration_test_value';

COMMENT ON COLUMN requests.test_migration_column IS 
    'Test column for validating deployment consistency during schema migrations. Can be safely removed.';

CREATE INDEX idx_requests_test_migration 
ON requests(test_migration_column)
WHERE test_migration_column IS NOT NULL;