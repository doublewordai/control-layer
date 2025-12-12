-- Add soft deletion support to users table for GDPR compliance
-- When a user is "deleted", we scrub their personal information but keep the record
-- to maintain referential integrity with credits_transactions and other audit tables
-- The updated_at timestamp will reflect when the deletion occurred

ALTER TABLE users
ADD COLUMN is_deleted BOOLEAN NOT NULL DEFAULT false;

-- Create an index for efficient filtering of active users
CREATE INDEX idx_users_is_deleted ON users (is_deleted) WHERE is_deleted = false;

-- Comments
COMMENT ON COLUMN users.is_deleted IS 'Soft deletion flag - when true, personal data has been scrubbed for GDPR compliance. Check updated_at for deletion timestamp.';
