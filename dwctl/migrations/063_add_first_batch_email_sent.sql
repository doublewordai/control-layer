-- Track whether the first-batch welcome email has been sent to a user.
-- This email is sent regardless of batch_notifications_enabled.
ALTER TABLE users ADD COLUMN first_batch_email_sent BOOLEAN NOT NULL DEFAULT false;
