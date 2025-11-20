-- Add external_user_id column to users table for proxy header authentication
-- This stores the unique user identifier from the upstream identity provider (IdP)
-- Examples: "auth0|google-oauth2|123456", "okta|abc123", etc.

ALTER TABLE users ADD COLUMN external_user_id TEXT;

-- Create unique index on external_user_id for fast lookups
-- Use partial index to allow multiple NULL values (for native auth users)
CREATE UNIQUE INDEX IF NOT EXISTS idx_users_external_user_id
ON users (external_user_id) WHERE external_user_id IS NOT NULL;

-- Add index for proxy-header users who have external_user_id set
CREATE INDEX IF NOT EXISTS idx_users_auth_source_external_id
ON users (auth_source, external_user_id) WHERE external_user_id IS NOT NULL;
