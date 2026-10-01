-- no-transaction
-- Reverting returns the template purges to scanning all of files.
DROP INDEX CONCURRENTLY IF EXISTS idx_files_explicitly_deleted;
