-- Add capacity fields to deployed_models for concurrent request limits

-- Add capacity columns to deployed_models table
ALTER TABLE deployed_models
ADD COLUMN capacity INTEGER DEFAULT NULL,
ADD COLUMN batch_capacity INTEGER DEFAULT NULL;

-- Add comments for clarity
COMMENT ON COLUMN deployed_models.capacity IS 'Maximum number of concurrent requests allowed for this model (null = no limit)';
COMMENT ON COLUMN deployed_models.batch_capacity IS 'Maximum number of concurrent batch requests allowed for this model (null = defaults to capacity or no limit)';

-- Trigger auth config change notification for onwards config sync
NOTIFY auth_config_changed, 'model_capacity_added';
