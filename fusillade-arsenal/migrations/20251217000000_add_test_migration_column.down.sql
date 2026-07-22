-- Rollback test migration: Remove dummy column from requests table

DROP INDEX IF EXISTS idx_requests_test_migration;

ALTER TABLE requests
DROP COLUMN IF EXISTS test_migration_column;