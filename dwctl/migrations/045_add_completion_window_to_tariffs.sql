-- Add completion_window column to model_tariffs for SLA-specific batch pricing
-- This allows multiple batch tariffs per model (one per SLA like "24h", "1h", etc.)
-- while maintaining single tariffs for realtime and playground purposes

-- Add the new column (nullable initially to support existing data)
ALTER TABLE model_tariffs
ADD COLUMN IF NOT EXISTS completion_window VARCHAR(50);

-- Drop the existing unique constraint that prevents multiple batch tariffs
DROP INDEX IF EXISTS idx_model_tariffs_unique_active_per_purpose;

-- Create new constraint for batch tariffs: unique per (model, purpose, completion_window)
-- This allows multiple batch tariffs per model as long as they have different completion_windows
CREATE UNIQUE INDEX IF NOT EXISTS idx_model_tariffs_unique_active_batch_per_sla
    ON model_tariffs(deployed_model_id, api_key_purpose, completion_window)
    WHERE valid_until IS NULL
      AND api_key_purpose = 'batch'
      AND completion_window IS NOT NULL;

-- Create constraint for realtime tariffs: still max one per model
CREATE UNIQUE INDEX IF NOT EXISTS idx_model_tariffs_unique_active_realtime
    ON model_tariffs(deployed_model_id, api_key_purpose)
    WHERE valid_until IS NULL
      AND api_key_purpose = 'realtime';

-- Create constraint for playground tariffs: still max one per model
CREATE UNIQUE INDEX IF NOT EXISTS idx_model_tariffs_unique_active_playground
    ON model_tariffs(deployed_model_id, api_key_purpose)
    WHERE valid_until IS NULL
      AND api_key_purpose = 'playground';

-- Create constraint for platform tariffs: still max one per model
CREATE UNIQUE INDEX IF NOT EXISTS idx_model_tariffs_unique_active_platform
    ON model_tariffs(deployed_model_id, api_key_purpose)
    WHERE valid_until IS NULL
      AND api_key_purpose = 'platform';

-- Note: Legacy tariffs (api_key_purpose = NULL) continue to have no uniqueness constraint

-- Update any existing batch tariffs that have NULL completion_window
-- Set them to "24h" which was the original default SLA
UPDATE model_tariffs
SET completion_window = '24h'
WHERE api_key_purpose = 'batch' AND completion_window IS NULL;

-- Enforce that batch tariffs must have a completion_window
-- This prevents creating batch tariffs without an SLA
ALTER TABLE model_tariffs
DROP CONSTRAINT IF EXISTS batch_tariffs_must_have_completion_window;

ALTER TABLE model_tariffs
ADD CONSTRAINT batch_tariffs_must_have_completion_window
CHECK (api_key_purpose != 'batch' OR completion_window IS NOT NULL);
