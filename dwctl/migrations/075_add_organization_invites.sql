-- Extend user_organizations with invite support.
-- Pending invites are stored as rows with status = 'pending'.
-- When the invite is accepted, status becomes 'active' and user_id is set.

-- Status column: 'active' for current members, 'pending' for unaccepted invites.
ALTER TABLE user_organizations
  ADD COLUMN status VARCHAR NOT NULL DEFAULT 'active'
    CHECK (status IN ('active', 'pending'));

-- Token hash for the invite link (nullable, only for pending invites).
ALTER TABLE user_organizations
  ADD COLUMN invite_token_hash VARCHAR;

-- Email the invite was sent to (for pending invites where user may not exist yet).
ALTER TABLE user_organizations
  ADD COLUMN invite_email VARCHAR;

-- Who sent the invite.
ALTER TABLE user_organizations
  ADD COLUMN invited_by UUID REFERENCES users(id);

-- When the invite expires.
ALTER TABLE user_organizations
  ADD COLUMN expires_at TIMESTAMPTZ;

-- Allow NULL user_id for pending invites where the invited user doesn't have an account yet.
ALTER TABLE user_organizations ALTER COLUMN user_id DROP NOT NULL;

-- Index for looking up invites by token hash (used during accept/decline).
CREATE INDEX idx_user_organizations_invite_token
  ON user_organizations(invite_token_hash) WHERE invite_token_hash IS NOT NULL;

-- Prevent duplicate pending invites for the same email + org.
CREATE UNIQUE INDEX idx_user_organizations_invite_email_org
  ON user_organizations(invite_email, organization_id)
  WHERE invite_email IS NOT NULL AND status = 'pending';

-- Update the trigger to allow NULL user_id for pending invites.
CREATE OR REPLACE FUNCTION check_organization_membership_types() RETURNS trigger AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM users WHERE id = NEW.organization_id AND user_type = 'organization'
    ) THEN
        RAISE EXCEPTION 'organization_id must reference a user with user_type = ''organization'''
            USING ERRCODE = 'check_violation';
    END IF;
    -- Only check user_id type if user_id is not null (pending invites may have null user_id).
    IF NEW.user_id IS NOT NULL AND NOT EXISTS (
        SELECT 1 FROM users WHERE id = NEW.user_id AND user_type = 'individual'
    ) THEN
        RAISE EXCEPTION 'user_id must reference a user with user_type = ''individual'''
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
