-- Add user_type column to distinguish individuals from organizations.
-- Default 'individual' preserves all existing behavior.
ALTER TABLE users
  ADD COLUMN user_type VARCHAR NOT NULL DEFAULT 'individual';
ALTER TABLE users
  ADD CONSTRAINT chk_user_type CHECK (user_type IN ('individual', 'organization'));

-- Mapping: which users belong to which organizations.
-- An organization is itself a row in the users table with user_type = 'organization'.
CREATE TABLE user_organizations (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    organization_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role VARCHAR NOT NULL DEFAULT 'member' CHECK (role IN ('owner', 'admin', 'member')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (user_id, organization_id)
);

CREATE INDEX idx_user_organizations_user_id ON user_organizations(user_id);
CREATE INDEX idx_user_organizations_organization_id ON user_organizations(organization_id);

-- Enforce that organization_id points to an 'organization' user and user_id points to an 'individual' user.
-- CHECK constraints can't query other tables, so we use a trigger.
CREATE OR REPLACE FUNCTION check_organization_membership_types() RETURNS trigger AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM users WHERE id = NEW.organization_id AND user_type = 'organization'
    ) THEN
        RAISE EXCEPTION 'organization_id must reference a user with user_type = ''organization'''
            USING ERRCODE = 'check_violation';
    END IF;
    IF NOT EXISTS (
        SELECT 1 FROM users WHERE id = NEW.user_id AND user_type = 'individual'
    ) THEN
        RAISE EXCEPTION 'user_id must reference a user with user_type = ''individual'''
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER enforce_organization_membership_types
    BEFORE INSERT OR UPDATE ON user_organizations
    FOR EACH ROW EXECUTE FUNCTION check_organization_membership_types();

-- Trigger NOTIFY so onwards picks up any future config changes involving org users
CREATE TRIGGER user_organizations_notify
    AFTER INSERT OR UPDATE OR DELETE ON user_organizations
    FOR EACH STATEMENT EXECUTE FUNCTION notify_config_change();

-- Add api_key_id to http_analytics for per-key usage attribution within orgs.
-- Currently only user_id is tracked; this enables "which member's key caused this usage?"
-- NOTE: ALTER TABLE ADD COLUMN with no DEFAULT is metadata-only (instant, no table rewrite).
-- CREATE INDEX on 20M+ rows takes ~30-90s and holds a SHARE lock (blocks INSERTs).
-- This is acceptable: inference requests still flow, only analytics writes queue briefly.
-- The transactional migration ensures full rollback on failure (no orphaned state).
ALTER TABLE http_analytics ADD COLUMN api_key_id UUID;
CREATE INDEX idx_http_analytics_api_key_id ON http_analytics(api_key_id);

-- Add api_key_id and performed_by to credits_transactions.
-- api_key_id: direct attribution for usage deductions — avoids joining through http_analytics.
-- performed_by: audit trail for management actions (e.g., admin granting credits).
-- Both are NULL for legacy rows and system-generated transactions.
ALTER TABLE credits_transactions ADD COLUMN api_key_id UUID;
CREATE INDEX idx_credits_transactions_api_key_id ON credits_transactions(api_key_id) WHERE api_key_id IS NOT NULL;
ALTER TABLE credits_transactions ADD COLUMN performed_by UUID REFERENCES users(id);

-- Soft-delete for api_keys. Hard delete would orphan api_key_id references in
-- credits_transactions and http_analytics, losing attribution data.
-- Mirrors the existing soft-delete pattern on the users table.
ALTER TABLE api_keys ADD COLUMN is_deleted BOOLEAN NOT NULL DEFAULT FALSE;
CREATE INDEX idx_api_keys_is_deleted ON api_keys(user_id) WHERE is_deleted = false;

-- Track which individual user created each API key.
-- For individual users: created_by = user_id (self-created).
-- For org keys: created_by = the member who created it, user_id = the org.
ALTER TABLE api_keys ADD COLUMN created_by UUID REFERENCES users(id);

-- Backfill: all existing keys were created by the user who owns them.
UPDATE api_keys SET created_by = user_id;

-- Now make it NOT NULL for all future inserts.
ALTER TABLE api_keys ALTER COLUMN created_by SET NOT NULL;

-- Update hidden key unique constraint: one hidden key per (user_id, created_by, purpose).
-- Old: (user_id, purpose) WHERE hidden = true — one per user per purpose.
-- New: (user_id, created_by, purpose) WHERE hidden = true — one per org member per purpose.
-- For individuals, created_by = user_id, so behavior is unchanged.
DROP INDEX idx_api_keys_user_hidden_purpose;
CREATE UNIQUE INDEX idx_api_keys_user_hidden_purpose
  ON api_keys(user_id, created_by, purpose) WHERE hidden = true AND is_deleted = false;

-- Update the unique name constraint to exclude soft-deleted keys.
-- Without this, recreating a key with the same name after soft-delete would fail.
ALTER TABLE api_keys DROP CONSTRAINT api_keys_user_id_name_unique;
CREATE UNIQUE INDEX api_keys_user_id_name_unique ON api_keys(user_id, name) WHERE is_deleted = false;
