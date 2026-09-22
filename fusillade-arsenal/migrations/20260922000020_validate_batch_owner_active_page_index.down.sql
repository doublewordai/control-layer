-- Remove the index comment. Reverts before 20260922000000 drops the index
-- itself, so the target still exists at this point.

COMMENT ON INDEX idx_batches_owner_active_created_at_id IS NULL;