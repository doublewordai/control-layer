DROP INDEX IF EXISTS idx_batches_pending_notification;
ALTER TABLE batches DROP COLUMN IF EXISTS notification_sent_at;
