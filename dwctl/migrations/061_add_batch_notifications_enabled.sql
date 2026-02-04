-- Add per-user toggle for batch completion email notifications
ALTER TABLE users ADD COLUMN batch_notifications_enabled BOOLEAN NOT NULL DEFAULT false;
