-- Add with-replacement and max-attempts options for weighted random failover.
-- fallback_with_replacement defaults to FALSE (preserves existing behaviour:
-- without replacement); fallback_max_attempts defaults to NULL (attempt count
-- = provider count).

ALTER TABLE deployed_models
  ADD COLUMN fallback_with_replacement BOOLEAN DEFAULT FALSE,
  ADD COLUMN fallback_max_attempts INTEGER DEFAULT NULL;
