-- Per-membership additive roles for organizations.
--
-- Org membership carries a base role (owner/admin/member) on
-- user_organizations; this table adds optional org-scoped roles on top,
-- mirroring the additive user_roles pattern at membership scope. A grant
-- attaches to the membership row (including pending invites, so invite-time
-- pre-configuration survives acceptance) and disappears with it via CASCADE.
--
-- Roles:
--   'manage_keys' — the member may create and manage their own API keys in
--     this org. Owners/admins (and personal accounts) have this implicitly
--     and never need a row. Members without it hold issued keys: they can
--     view and rotate them (rotation doubles as secret recovery) but not
--     create, re-cap, or delete.
CREATE TABLE organization_member_roles (
    user_organization_id UUID NOT NULL REFERENCES user_organizations(id) ON DELETE CASCADE,
    role VARCHAR NOT NULL CHECK (role IN ('manage_keys')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (user_organization_id, role)
);

-- Existing plain members (and pending invites) keep today's self-serve
-- behavior; new memberships default to granted at the API layer unless the
-- inviter opts out.
INSERT INTO organization_member_roles (user_organization_id, role)
SELECT id, 'manage_keys' FROM user_organizations WHERE role = 'member';
