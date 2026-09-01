-- Add api_key to batches so the batch creator's key is used for execution,
-- not the file uploader's key from request_templates.
-- Nullable for backwards compatibility with existing batches.
ALTER TABLE batches ADD COLUMN api_key TEXT;
