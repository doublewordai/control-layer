-- Remove test migration column that was added for deployment testing

DROP INDEX IF EXISTS idx_requests_test_migration;

ALTER TABLE requests
DROP COLUMN IF EXISTS test_migration_column;