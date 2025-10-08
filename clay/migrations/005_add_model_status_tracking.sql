-- Add status tracking fields to deployed_models
ALTER TABLE deployed_models 
ADD COLUMN status VARCHAR NOT NULL DEFAULT 'active',
ADD COLUMN last_sync TIMESTAMPTZ,
ADD COLUMN deleted BOOLEAN NOT NULL DEFAULT false;

-- Create index on status for efficient filtering
CREATE INDEX idx_deployed_models_status ON deployed_models(status);

-- Create index on last_sync for monitoring queries
CREATE INDEX idx_deployed_models_last_sync ON deployed_models(last_sync);

-- Create index on deleted for efficient visibility filtering
CREATE INDEX idx_deployed_models_deleted ON deployed_models(deleted);

-- Update existing models to have a last_sync timestamp
UPDATE deployed_models SET last_sync = updated_at WHERE last_sync IS NULL;