-- Backfill organizations from existing users' email domains.
--
-- For each business email domain (excluding personal/service providers):
-- 1. If an org already exists where an owner's email matches the domain,
--    add any unaffiliated users from that domain as members.
-- 2. If no org exists for the domain, create one with the earliest user
--    as owner and all others as members.
--
-- DRY RUN: wrap in a transaction and ROLLBACK to preview changes.
-- APPLY:   change ROLLBACK to COMMIT at the bottom.

BEGIN;

-- Personal/service email domains to exclude (matches auth/utils.rs list)
CREATE TEMP TABLE personal_domains (domain TEXT PRIMARY KEY);
INSERT INTO personal_domains VALUES
  -- Major providers
  ('gmail.com'),('googlemail.com'),('hotmail.com'),('hotmail.co.uk'),
  ('live.com'),('live.fr'),('outlook.com'),('msn.com'),
  ('yahoo.com'),('yahoo.co.uk'),('yahoo.co.jp'),('ymail.com'),
  ('aol.com'),('aim.com'),('icloud.com'),('me.com'),('mac.com'),
  ('mail.com'),('zoho.com'),('yandex.com'),('163.com'),
  -- Privacy-focused
  ('protonmail.com'),('protonmail.ch'),('proton.me'),
  ('tutanota.com'),('tuta.com'),('fastmail.com'),
  -- Regional/misc
  ('gmx.com'),('gmx.de'),('gmx.net'),('gbnet.net'),
  -- Privacy relays and aliases
  ('privaterelay.appleid.com'),('mozmail.com'),('duck.com'),('passmail.net'),
  -- Internal/system
  ('internal'),('notifications.doubleword.ai');

-------------------------------------------------------------------------------
-- Step 1: Build a mapping of domain → existing org (if any)
--
-- An org "covers" a domain if it has an active, non-deleted owner whose
-- email domain matches. Pick the org with the most members if multiple match.
-------------------------------------------------------------------------------
CREATE TEMP TABLE domain_org_map AS
SELECT DISTINCT ON (domain)
  split_part(owner.email, '@', 2) AS domain,
  org.id AS org_id,
  org.username AS org_username
FROM users org
JOIN user_organizations uo ON uo.organization_id = org.id AND uo.role = 'owner' AND uo.status = 'active'
JOIN users owner ON owner.id = uo.user_id AND owner.is_deleted = false
WHERE org.user_type = 'organization'
  AND org.is_deleted = false
ORDER BY domain, (SELECT COUNT(*) FROM user_organizations WHERE organization_id = org.id) DESC;

\echo '=== Domains already covered by existing orgs ==='
SELECT domain, org_username, org_id FROM domain_org_map ORDER BY domain;

-------------------------------------------------------------------------------
-- Step 2: Collect all business-domain users who need org membership
--         (includes single-user domains)
-------------------------------------------------------------------------------
CREATE TEMP TABLE domain_users AS
SELECT
  u.id AS user_id,
  u.email,
  split_part(u.email, '@', 2) AS domain,
  u.created_at,
  ROW_NUMBER() OVER (PARTITION BY split_part(u.email, '@', 2) ORDER BY u.created_at ASC) AS rank
FROM users u
WHERE u.user_type = 'individual'
  AND u.is_deleted = false
  AND u.auth_source = 'proxy-header'
  AND u.external_user_id IS NOT NULL
  AND split_part(u.email, '@', 2) NOT IN (SELECT domain FROM personal_domains);

\echo '=== Users to be assigned to orgs (by domain) ==='
SELECT domain, COUNT(*) AS user_count FROM domain_users GROUP BY domain ORDER BY user_count DESC, domain;

-------------------------------------------------------------------------------
-- Step 3: Create new orgs for domains that don't have one yet
-- The first user (by created_at) becomes the owner.
-------------------------------------------------------------------------------
\echo '=== Creating new orgs for uncovered domains ==='
INSERT INTO users (id, username, email, auth_source, user_type, is_admin)
SELECT
  gen_random_uuid(),
  du.domain,                          -- username = domain
  du.email,                           -- contact email = first user's email
  'organization',
  'organization',
  false
FROM domain_users du
WHERE du.rank = 1
  AND du.domain NOT IN (SELECT domain FROM domain_org_map)
  -- Don't collide with existing username
  AND NOT EXISTS (SELECT 1 FROM users u2 WHERE u2.username = du.domain AND u2.is_deleted = false)
RETURNING id, username, email;

-- Give new orgs StandardUser + BatchAPIUser roles (matches default_user_roles)
INSERT INTO user_roles (user_id, role)
SELECT u.id, r.role
FROM users u
CROSS JOIN (VALUES ('STANDARDUSER'::user_role), ('BATCHAPIUSER'::user_role)) AS r(role)
WHERE u.user_type = 'organization'
  AND u.username IN (SELECT domain FROM domain_users WHERE rank = 1 AND domain NOT IN (SELECT domain FROM domain_org_map))
  AND NOT EXISTS (SELECT 1 FROM user_roles ur WHERE ur.user_id = u.id AND ur.role = r.role);

-- Update domain_org_map with newly created orgs
INSERT INTO domain_org_map (domain, org_id, org_username)
SELECT u.username, u.id, u.username
FROM users u
WHERE u.user_type = 'organization'
  AND u.is_deleted = false
  AND u.username IN (SELECT domain FROM domain_users WHERE rank = 1 AND domain NOT IN (SELECT domain FROM domain_org_map));

-------------------------------------------------------------------------------
-- Step 4: Add owner memberships (rank=1 user for each new org)
-------------------------------------------------------------------------------
\echo '=== Adding org owners ==='
INSERT INTO user_organizations (user_id, organization_id, role, status)
SELECT du.user_id, dom.org_id, 'owner', 'active'
FROM domain_users du
JOIN domain_org_map dom ON dom.domain = du.domain
WHERE du.rank = 1
  -- Only for newly created orgs (existing orgs already have owners)
  AND dom.domain NOT IN (
    SELECT split_part(owner.email, '@', 2)
    FROM user_organizations uo
    JOIN users owner ON owner.id = uo.user_id AND owner.is_deleted = false
    WHERE uo.organization_id = dom.org_id AND uo.role = 'owner' AND uo.status = 'active'
  )
  -- Don't duplicate
  AND NOT EXISTS (
    SELECT 1 FROM user_organizations uo2
    WHERE uo2.user_id = du.user_id AND uo2.organization_id = dom.org_id
  )
RETURNING user_id, organization_id;

-------------------------------------------------------------------------------
-- Step 5: Add member memberships (all users not already in their domain's org)
-------------------------------------------------------------------------------
\echo '=== Adding org members ==='
INSERT INTO user_organizations (user_id, organization_id, role, status)
SELECT du.user_id, dom.org_id, 'member', 'active'
FROM domain_users du
JOIN domain_org_map dom ON dom.domain = du.domain
WHERE NOT EXISTS (
    SELECT 1 FROM user_organizations uo2
    WHERE uo2.user_id = du.user_id AND uo2.organization_id = dom.org_id
  )
RETURNING user_id, organization_id;

-------------------------------------------------------------------------------
-- Summary
-------------------------------------------------------------------------------
\echo '=== Final summary ==='
SELECT
  dom.domain,
  dom.org_username,
  COUNT(*) FILTER (WHERE uo.role = 'owner') AS owners,
  COUNT(*) FILTER (WHERE uo.role = 'member') AS members,
  COUNT(*) AS total
FROM domain_org_map dom
JOIN user_organizations uo ON uo.organization_id = dom.org_id AND uo.status = 'active'
JOIN users u ON u.id = uo.user_id AND u.is_deleted = false
GROUP BY dom.domain, dom.org_username
ORDER BY total DESC, dom.domain;

-- DRY RUN: ROLLBACK to preview without making changes
-- APPLY:   Change to COMMIT
ROLLBACK;
