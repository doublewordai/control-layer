ALTER TABLE users ADD COLUMN auto_topup_monthly_limit REAL DEFAULT NULL;
ALTER TABLE users ADD COLUMN auto_topup_limit_notification_sent BOOLEAN NOT NULL DEFAULT FALSE;
