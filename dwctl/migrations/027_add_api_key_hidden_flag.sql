-- Add 'hidden' flag to api_keys table
-- Hidden API keys are user-specific internal keys used by the system for proxying requests
-- They are not shown in list endpoints and are managed automatically

ALTER TABLE api_keys ADD COLUMN hidden BOOLEAN NOT NULL DEFAULT false;

-- Create index for efficient querying of non-hidden keys
CREATE INDEX idx_api_keys_hidden ON api_keys(hidden) WHERE hidden = false;

-- Create unique constraint to ensure only one hidden key per user per purpose
CREATE UNIQUE INDEX idx_api_keys_user_hidden_purpose ON api_keys(user_id, purpose) WHERE hidden = true;
