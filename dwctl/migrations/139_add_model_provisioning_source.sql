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
CREATE UNIQUE INDEX idx_model_tariffs_unique_active_continuation
    ON model_tariffs (deployed_model_id, api_key_purpose)
    WHERE valid_until IS NULL AND api_key_purpose = 'continuation';
