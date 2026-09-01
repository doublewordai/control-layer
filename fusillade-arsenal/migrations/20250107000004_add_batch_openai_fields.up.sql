-- Add OpenAI-compatible fields to batches table

-- Add metadata for storing key-value pairs (OpenAI allows 16 pairs)
ALTER TABLE batches ADD COLUMN metadata JSONB DEFAULT '{}'::jsonb;

-- Add completion_window (e.g., "24h")
ALTER TABLE batches ADD COLUMN completion_window TEXT NOT NULL DEFAULT '24h';

-- Add endpoint to track which API endpoint the batch uses
ALTER TABLE batches ADD COLUMN endpoint TEXT NOT NULL DEFAULT '/v1/chat/completions';

-- Add output_file_id for successful results
ALTER TABLE batches ADD COLUMN output_file_id UUID NULL REFERENCES files(id) ON DELETE SET NULL;

-- Add error_file_id for failed results
ALTER TABLE batches ADD COLUMN error_file_id UUID NULL REFERENCES files(id) ON DELETE SET NULL;

-- Add indexes for the new foreign keys
CREATE INDEX idx_batches_output_file_id ON batches(output_file_id) WHERE output_file_id IS NOT NULL;
CREATE INDEX idx_batches_error_file_id ON batches(error_file_id) WHERE error_file_id IS NOT NULL;
