-- Org-level key management mode, the policy switch for in-org API key
-- governance ("Managed keys"):
--
--   'open'    (default, = pre-existing behavior): members create and fully
--             manage their OWN keys — including setting/editing/resetting
--             usage limits on them (caps are an owner's tool, e.g. budgeting
--             an agent's key). Org owners/admins additionally see and manage
--             all org keys.
--   'managed': members cannot create keys at all. Every key they hold was
--              issued to them by an org owner/admin (created with member_id
--              attribution) and is READ-ONLY to them: they can view usage /
--              window / reset time and fetch the secret, nothing else.
--              Dashboard-created batches must select an issued key so UI
--              usage cannot bypass key budgets.
--
-- Organizations are rows in `users` (user_type = 'organization'), so the
-- setting lives here; the CHECK pins individual accounts to 'open' so the
-- column is meaningless-but-harmless for them. Enforcement happens in the
-- capability layer (auth/permissions.rs), NOT in SQL.
ALTER TABLE users
  ADD COLUMN org_key_management_mode VARCHAR NOT NULL DEFAULT 'open',
  ADD CONSTRAINT chk_org_key_management_mode
    CHECK (org_key_management_mode IN ('open', 'managed')),
  ADD CONSTRAINT chk_org_key_management_mode_orgs_only
    CHECK (user_type = 'organization' OR org_key_management_mode = 'open');

COMMENT ON COLUMN users.org_key_management_mode IS
  'Key governance policy for organization rows: ''open'' = members self-serve '
  'and manage their own keys (default, pre-existing behavior); ''managed'' = '
  'only org owners/admins create/manage keys, members read+use issued keys. '
  'Always ''open'' for individual accounts.';
