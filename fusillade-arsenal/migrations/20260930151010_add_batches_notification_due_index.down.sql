-- no-transaction
-- Reverting returns the notification claim to walking idx_batches_retention_due.
DROP INDEX CONCURRENTLY IF EXISTS idx_batches_notification_due;
