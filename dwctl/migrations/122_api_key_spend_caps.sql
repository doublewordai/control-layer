-- Per-API-key spending caps: schema.
--
-- A spending cap is an optional property of a user-visible API key K. Because
-- batch and flex traffic executes on hidden batch-purpose keys (not K itself),
-- capping K requires a "cap scope": K plus one hidden batch-purpose CHILD key
-- minted for K (parent_api_key_id = K.id). Realtime traffic uses K directly;
-- batch/flex requests authenticated with K execute on the child. Spend
-- accounting folds both keys' spend into one checkpoint row keyed by the scope
-- root, and cap enforcement yanks the whole scope from the onwards key set.
-- Uncapped keys are untouched: they keep resolving to the shared per-member
-- hidden keys exactly as before.

ALTER TABLE api_keys
  ADD COLUMN spend_limit          DECIMAL(20,9) NULL,
  ADD COLUMN spend_limit_interval TEXT          NULL,
  ADD COLUMN parent_api_key_id    UUID          NULL REFERENCES api_keys(id),
  ADD CONSTRAINT api_keys_spend_limit_positive
    CHECK (spend_limit IS NULL OR spend_limit > 0),
  -- Windows are CALENDAR-ALIGNED (UTC), not rolling: NULL interval = one-off
  -- cap ("spend since the cap was set/reset"), otherwise the window resets at
  -- the UTC day/week/month boundary. No window end is stored anywhere — window
  -- membership is a date_trunc comparison against window_started_at.
  ADD CONSTRAINT api_keys_spend_limit_interval_valid
    CHECK (spend_limit_interval IS NULL OR spend_limit_interval IN ('daily', 'weekly', 'monthly')),
  ADD CONSTRAINT api_keys_interval_requires_limit
    CHECK (spend_limit_interval IS NULL OR spend_limit IS NOT NULL),
  -- The cap always lives on the scope root: child keys never carry their own.
  ADD CONSTRAINT api_keys_children_carry_no_cap
    CHECK (parent_api_key_id IS NULL OR spend_limit IS NULL);

COMMENT ON COLUMN api_keys.parent_api_key_id IS
  'Cap-scope link: set only on hidden batch-purpose child keys minted for a '
  'capped visible key. Batch/flex traffic authenticated with the parent '
  'executes on the child so per-key spending caps cover all tiers. Spend '
  'accounting and cap enforcement always group by COALESCE(parent_api_key_id, id) '
  '(the scope root) — any future per-key spend aggregation MUST use the same '
  'expression. Children are soft-deleted with their parent and never appear in '
  'key listings.';

COMMENT ON COLUMN api_keys.spend_limit IS
  'Optional spending cap in credits for this key''s cap scope (this key plus '
  'its hidden batch child). Enforced post-hoc via the onwards config sync: the '
  'scope is excluded from the proxy key set once the checkpoint''s window_spend '
  'reaches this limit. NULL = uncapped. Always NULL on child keys.';

COMMENT ON COLUMN api_keys.spend_limit_interval IS
  'Cap reset period: NULL = one-off (never resets automatically), else '
  'daily/weekly/monthly on CALENDAR-ALIGNED UTC boundaries (not rolling '
  'windows). Window membership = date_trunc(unit, window_started_at) vs now().';

-- The shared hidden-key uniqueness (one per (user_id, created_by, purpose))
-- now applies only to parentless keys; cap-scope children get their own
-- uniqueness (one child per (parent, purpose)). NOTE: the ON CONFLICT
-- inference specs in db/handlers/api_keys.rs name these predicates and must
-- stay in lockstep with them.
DROP INDEX idx_api_keys_user_hidden_purpose;
CREATE UNIQUE INDEX idx_api_keys_user_hidden_purpose
  ON api_keys(user_id, created_by, purpose)
  WHERE hidden = true AND is_deleted = false AND parent_api_key_id IS NULL;

CREATE UNIQUE INDEX idx_api_keys_child_purpose
  ON api_keys(parent_api_key_id, purpose)
  WHERE hidden = true AND is_deleted = false AND parent_api_key_id IS NOT NULL;

-- Per-cap-scope running spend totals, maintained by the analytics batcher in
-- the same transaction as credits_transactions / user_balance_checkpoints
-- (mirroring the user-balance checkpoint pattern: O(1) reads, no ledger
-- aggregation at read or sync time). Rows exist only for keys in a cap scope.
CREATE TABLE api_key_spend_checkpoints (
  -- The cap-scope ROOT key (the visible capped key), never a child id.
  api_key_id        UUID PRIMARY KEY REFERENCES api_keys(id) ON DELETE CASCADE,
  -- Lifetime total for the scope (monotonic; display only).
  total_spend       DECIMAL(20,9) NOT NULL DEFAULT 0,
  -- Spend inside the current cap window. The batcher fold rolls this over
  -- lazily: the first billed request after a CALENDAR boundary replaces the
  -- stale value instead of accumulating. Enforcement never depends on the
  -- rollover having happened — the sync predicate checks window membership
  -- itself (calendar-aligned date_trunc comparison, see api_keys columns).
  window_spend      DECIMAL(20,9) NOT NULL DEFAULT 0,
  window_started_at TIMESTAMPTZ   NOT NULL DEFAULT NOW(),
  updated_at        TIMESTAMPTZ   NOT NULL DEFAULT NOW()
);

COMMENT ON TABLE api_key_spend_checkpoints IS
  'Running spend per API-key cap scope, keyed by the scope root '
  '(COALESCE(parent_api_key_id, id) of the billed key). Folded by the '
  'analytics batcher transactionally with the credits ledger. Cap enforcement '
  'reads window_spend; window boundaries are calendar-aligned UTC and are not '
  'stored — see api_keys.spend_limit_interval.';

-- Extend the scoped api_keys UPDATE-notify (migration 101) with the cap
-- columns: cap edits change onwards key-set eligibility, so they must trigger
-- a config reload. parent_api_key_id is immutable after creation and children
-- are created/deleted whole rows (INSERT/DELETE triggers already notify), but
-- it is included for safety — a parent change would change sync eligibility.
CREATE OR REPLACE FUNCTION notify_api_keys_config_change() RETURNS trigger AS $$
DECLARE
    relevant_change boolean := false;
BEGIN
    IF TG_OP = 'INSERT' THEN
        relevant_change := EXISTS (SELECT 1 FROM new_rows);
    ELSIF TG_OP = 'DELETE' THEN
        relevant_change := EXISTS (SELECT 1 FROM old_rows);
    ELSIF TG_OP = 'UPDATE' THEN
        -- Only columns the sync query reads matter; metadata-only updates
        -- (name, description, last_used) must not reload the cache. Joined on the
        -- immutable primary key.
        relevant_change := EXISTS (
            SELECT 1
            FROM new_rows n
            JOIN old_rows o ON o.id = n.id
            WHERE o.secret               IS DISTINCT FROM n.secret
               OR o.purpose              IS DISTINCT FROM n.purpose
               OR o.user_id              IS DISTINCT FROM n.user_id
               OR o.requests_per_second  IS DISTINCT FROM n.requests_per_second
               OR o.burst_size           IS DISTINCT FROM n.burst_size
               OR o.is_deleted           IS DISTINCT FROM n.is_deleted
               OR o.hidden               IS DISTINCT FROM n.hidden
               OR o.spend_limit          IS DISTINCT FROM n.spend_limit
               OR o.spend_limit_interval IS DISTINCT FROM n.spend_limit_interval
               OR o.parent_api_key_id    IS DISTINCT FROM n.parent_api_key_id
        );
    END IF;

    IF relevant_change THEN
        -- Match notify_config_change()'s payload format (migration 049) so the
        -- cache-sync lag metric keeps attributing reloads to the api_keys table.
        PERFORM pg_notify('auth_config_changed',
            'api_keys:' || (extract(epoch FROM clock_timestamp()) * 1000000)::bigint::text);
    END IF;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;
