-- no-transaction
-- Reverting returns the file-content expiry sweep to scanning files.
DROP INDEX CONCURRENTLY IF EXISTS idx_files_content_expiry_due;
