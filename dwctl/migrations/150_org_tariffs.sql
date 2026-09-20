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

SET LOCAL lock_timeout = '5s';
-- Small configuration ledgers; bound index/constraint execution as well as lock acquisition.
SET LOCAL statement_timeout = '10s';

-- ---------------------------------------------------------------------------------------
-- model_tariffs
-- ---------------------------------------------------------------------------------------
ALTER TABLE model_tariffs
    ADD COLUMN user_id UUID REFERENCES users(id) ON DELETE CASCADE;

COMMENT ON COLUMN model_tariffs.user_id IS
  'Organisation this tariff belongs to; NULL = the model''s general price. Billing prefers the caller''s organisation rows over the general rows for the same purpose and completion window.';

CREATE INDEX idx_model_tariffs_user_id ON model_tariffs (user_id) WHERE user_id IS NOT NULL;

-- General scope: the original indexes, narrowed to user_id IS NULL.
DROP INDEX IF EXISTS idx_model_tariffs_unique_active_batch_per_sla;
CREATE UNIQUE INDEX idx_model_tariffs_unique_active_batch_per_sla
    ON model_tariffs (deployed_model_id, api_key_purpose, completion_window)
    WHERE valid_until IS NULL AND user_id IS NULL
      AND api_key_purpose = 'batch' AND completion_window IS NOT NULL;

DROP INDEX IF EXISTS idx_model_tariffs_unique_active_realtime;
CREATE UNIQUE INDEX idx_model_tariffs_unique_active_realtime
    ON model_tariffs (deployed_model_id, api_key_purpose)
    WHERE valid_until IS NULL AND user_id IS NULL AND api_key_purpose = 'realtime';

DROP INDEX IF EXISTS idx_model_tariffs_unique_active_playground;
CREATE UNIQUE INDEX idx_model_tariffs_unique_active_playground
    ON model_tariffs (deployed_model_id, api_key_purpose)
    WHERE valid_until IS NULL AND user_id IS NULL AND api_key_purpose = 'playground';

DROP INDEX IF EXISTS idx_model_tariffs_unique_active_platform;
CREATE UNIQUE INDEX idx_model_tariffs_unique_active_platform
    ON model_tariffs (deployed_model_id, api_key_purpose)
    WHERE valid_until IS NULL AND user_id IS NULL AND api_key_purpose = 'platform';

DROP INDEX IF EXISTS idx_model_tariffs_unique_active_continuation;
CREATE UNIQUE INDEX idx_model_tariffs_unique_active_continuation
    ON model_tariffs (deployed_model_id, api_key_purpose)
    WHERE valid_until IS NULL AND user_id IS NULL AND api_key_purpose = 'continuation';

-- Organisation scope: one active row per (organisation, model, purpose[, window]).
CREATE UNIQUE INDEX idx_model_tariffs_unique_active_org_batch_per_sla
    ON model_tariffs (user_id, deployed_model_id, api_key_purpose, completion_window)
    WHERE valid_until IS NULL AND user_id IS NOT NULL
      AND api_key_purpose = 'batch' AND completion_window IS NOT NULL;

CREATE UNIQUE INDEX idx_model_tariffs_unique_active_org_per_purpose
    ON model_tariffs (user_id, deployed_model_id, api_key_purpose)
    WHERE valid_until IS NULL AND user_id IS NOT NULL
      AND api_key_purpose IN ('realtime', 'playground', 'platform', 'continuation');

-- ---------------------------------------------------------------------------------------
-- model_cache_tariffs
-- ---------------------------------------------------------------------------------------
ALTER TABLE model_cache_tariffs
    ADD COLUMN user_id UUID REFERENCES users(id) ON DELETE CASCADE;

COMMENT ON COLUMN model_cache_tariffs.user_id IS
  'Organisation these multipliers belong to; NULL = the model''s general cache pricing. An organisation row overrides the multipliers for that organisation''s requests; caching itself is enabled by the general row.';

CREATE INDEX idx_model_cache_tariffs_user_id ON model_cache_tariffs (user_id) WHERE user_id IS NOT NULL;

-- (model, valid_from) was unique; general and organisation rows written in the same
-- transaction share valid_from, so the guarantee becomes per scope.
ALTER TABLE model_cache_tariffs DROP CONSTRAINT model_cache_tariffs_deployed_model_id_valid_from_key;
CREATE UNIQUE INDEX idx_model_cache_tariffs_version_general
    ON model_cache_tariffs (deployed_model_id, valid_from) WHERE user_id IS NULL;
CREATE UNIQUE INDEX idx_model_cache_tariffs_version_org
    ON model_cache_tariffs (user_id, deployed_model_id, valid_from) WHERE user_id IS NOT NULL;

DROP INDEX IF EXISTS idx_model_cache_tariffs_unique_active;
CREATE UNIQUE INDEX idx_model_cache_tariffs_unique_active
    ON model_cache_tariffs (deployed_model_id)
    WHERE valid_until IS NULL AND user_id IS NULL;
CREATE UNIQUE INDEX idx_model_cache_tariffs_unique_active_org
    ON model_cache_tariffs (user_id, deployed_model_id)
    WHERE valid_until IS NULL AND user_id IS NOT NULL;
