-- Track whether the first-batch welcome email has been sent to a user.
-- This email is sent regardless of batch_notifications_enabled.
ALTER TABLE users ADD COLUMN first_batch_email_sent BOOLEAN NOT NULL DEFAULT false;

-- Mark existing users who have already submitted batches so they don't
-- receive a spurious "first batch" email after this migration.
UPDATE users SET first_batch_email_sent = true
WHERE id IN (SELECT DISTINCT user_id FROM batch_aggregates);
