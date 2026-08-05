-- Join requests: a user asking to join an organization, awaiting approval.
--
-- The mirror image of an invite, so it reuses the same membership lifecycle
-- rather than getting its own table. `pending` already means "the org invited
-- someone who hasn't accepted"; `requested` means "someone asked to join and
-- the org hasn't decided". Both become `active` on acceptance, and both are
-- already covered by the UNIQUE (user_id, organization_id) constraint, the
-- membership-type trigger and the existing indexes.
--
-- This replaces silent auto-join. Until now, first login with a company email
-- domain added the user straight into that domain's organization as a member
-- (see auth::current_user), with no approval and no notice to either party.
-- Anyone who could receive mail at the domain landed inside the org that owns
-- the billing account. They now land in this table instead, and an owner or
-- admin decides.
--
-- Auto-*creation* of an org for an unclaimed domain is unchanged and still
-- happens; gating that needs domain ownership proof (DNS TXT), which is a
-- separate piece of work.
ALTER TABLE user_organizations
    DROP CONSTRAINT IF EXISTS user_organizations_status_check;

ALTER TABLE user_organizations
    ADD CONSTRAINT user_organizations_status_check
        CHECK (status IN ('active', 'pending', 'requested'));

-- Owners/admins list these per organization; the existing
-- idx_user_organizations_organization_id covers the whole table, so this
-- partial index keeps the common "show me the queue" read cheap as membership
-- grows.
CREATE INDEX idx_user_organizations_requested
    ON user_organizations(organization_id, created_at)
    WHERE status = 'requested';

COMMENT ON COLUMN user_organizations.status IS
    'active = current member; pending = invited by the org, not yet accepted; requested = asked to join, awaiting approval.';
