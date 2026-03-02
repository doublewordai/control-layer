ALTER TABLE users ADD COLUMN low_balance_notification_sent BOOLEAN NOT NULL DEFAULT false;
ALTER TABLE users ADD COLUMN low_balance_threshold REAL DEFAULT NULL;
