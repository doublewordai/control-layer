-- Organisation-scoped tariffs.
--
-- A tariff row (token prices, prompt-cache multipliers) may belong to ONE organisation
-- (`user_id` set) or be the model's general price (`user_id` NULL). Billing tries the
-- caller's organisation rows first and falls back to the general rows, so a bespoke deal
-- is declared once per (organisation, model) in the per-organisation catalog instead of a
-- private alias per customer. Rows stay temporal ledgers exactly as before: an org row is
-- closed and succeeded, never edited.
--
-- Uniqueness of the ACTIVE row is enforced per scope: the existing partial unique indexes
-- now cover the general rows (`user_id IS NULL`) and a parallel set covers each
-- organisation's rows. NULL is not equal to NULL in a unique index, which is why the
-- general and organisation scopes need their own indexes rather than one wider key.

-- The pre-rollout migration Job bounds execution time.

-- ---------------------------------------------------------------------------------------
-- model_tariffs
-- ---------------------------------------------------------------------------------------
-- Bound lock waits so queued DDL does not block live billing and admission reads.
SET LOCAL lock_timeout = '5s';

ALTER TABLE model_tariffs
    ADD COLUMN user_id UUID REFERENCES users(id) ON DELETE CASCADE;

COMMENT ON COLUMN model_tariffs.user_id IS
  'Organisation this tariff belongs to; NULL = the model''s general price. Billing prefers the caller''s organisation rows over the general rows for the same purpose and completion window.';

-- model_cache_tariffs
-- ---------------------------------------------------------------------------------------
ALTER TABLE model_cache_tariffs
    ADD COLUMN user_id UUID REFERENCES users(id) ON DELETE CASCADE;

COMMENT ON COLUMN model_cache_tariffs.user_id IS
  'Organisation these multipliers belong to; NULL = the model''s general cache pricing. An organisation row overrides the multipliers for that organisation''s requests; caching itself is enabled by the general row.';

-- Scoped indexes are built, recovered and validated by the concurrent migrations.
-- Existing uniqueness guards stay in place until every replacement is valid.
