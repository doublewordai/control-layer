-- Identify models whose declarative fields are restored from the model catalog
-- at startup. NULL means that the row is managed manually/discovered at runtime.
ALTER TABLE deployed_models
ADD COLUMN provisioning_source TEXT;

COMMENT ON COLUMN deployed_models.provisioning_source IS
  'Declarative model catalog source. Changes to provisioned fields are restored from this source at startup; NULL means unprovisioned.';

CREATE INDEX idx_deployed_models_provisioning_source
    ON deployed_models (provisioning_source)
    WHERE provisioning_source IS NOT NULL;

-- Migration 132 introduced the continuation purpose after the original tariff
-- indexes were created. Give it the same one-active-version invariant as the
-- other non-batch purposes before the provisioner can manage it.
--
-- Older application versions could create more than one open continuation
-- tariff. Reconstruct their historical boundaries in version order before
-- adding the invariant so an existing duplicate cannot make this migration
-- fail. The UUID tie-breaker makes equal valid_from values deterministic; the
-- superseded row then has a zero-length validity interval.
WITH active_continuation_versions AS (
    SELECT
        id,
        LEAD(valid_from) OVER (
            PARTITION BY deployed_model_id, api_key_purpose
            ORDER BY valid_from, id
        ) AS successor_valid_from
    FROM model_tariffs
    WHERE valid_until IS NULL AND api_key_purpose = 'continuation'
)
UPDATE model_tariffs AS tariff
SET valid_until = versions.successor_valid_from
FROM active_continuation_versions AS versions
WHERE tariff.id = versions.id
  AND versions.successor_valid_from IS NOT NULL;

CREATE UNIQUE INDEX idx_model_tariffs_unique_active_continuation
    ON model_tariffs (deployed_model_id, api_key_purpose)
    WHERE valid_until IS NULL AND api_key_purpose = 'continuation';
