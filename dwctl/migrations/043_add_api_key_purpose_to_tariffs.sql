-- Add api_key_purpose to model_tariffs to support purpose-specific pricing
-- This allows charging different rates for batch vs realtime vs playground inference
-- Each tariff row applies to exactly one purpose (create multiple rows for multiple purposes)

-- Step 1: Add the api_key_purpose column (nullable)
ALTER TABLE model_tariffs
ADD COLUMN api_key_purpose VARCHAR(50);

-- Step 2: Migrate existing tariffs
-- Tariffs named 'batch' get purpose 'batch', all others get 'realtime' (the default)
UPDATE model_tariffs
SET api_key_purpose = CASE
    WHEN name = 'batch' THEN 'batch'
    ELSE 'realtime'
END;

-- Step 3: Leave column NULL-able (tariffs can optionally have no purpose)

-- Step 4: Update the unique constraint
-- Drop old constraint that enforced (deployed_model_id, name) uniqueness
DROP INDEX idx_model_tariffs_unique_active;

-- Create new constraint: max one active tariff per (model, purpose) combination
-- Only applies to tariffs WITH a purpose (WHERE api_key_purpose IS NOT NULL)
-- Tariffs without a purpose have no uniqueness constraint
CREATE UNIQUE INDEX idx_model_tariffs_unique_active_per_purpose
  ON model_tariffs(deployed_model_id, api_key_purpose)
  WHERE valid_until IS NULL AND api_key_purpose IS NOT NULL;

-- Step 5: Drop is_default column (no longer needed - 'realtime' is the default)
ALTER TABLE model_tariffs
DROP COLUMN is_default;

-- Step 6: Update comments
COMMENT ON COLUMN model_tariffs.api_key_purpose IS 'API key purpose this tariff applies to (realtime, batch, playground). Each active tariff must specify a purpose.';
COMMENT ON COLUMN model_tariffs.name IS 'Descriptive name for the tariff (e.g., "Standard Pricing", "Premium Tier"). Purely informational.';
COMMENT ON TABLE model_tariffs IS 'Pricing tariffs for deployed models per API key purpose. Each model can have different pricing for realtime, batch, and playground usage. Supports temporal validity for accurate historical chargeback.';
