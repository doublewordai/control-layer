-- Add text_value column to system_config table to support storing strings like public keys
ALTER TABLE system_config ADD COLUMN text_value TEXT;

-- Make value column nullable since we'll use either value or text_value
ALTER TABLE system_config ALTER COLUMN value DROP NOT NULL;
