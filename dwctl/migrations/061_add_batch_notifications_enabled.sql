-- Add per-user toggle for batch completion email notifications (default on)
ALTER TABLE users ADD COLUMN batch_notifications_enabled BOOLEAN NOT NULL DEFAULT true;
