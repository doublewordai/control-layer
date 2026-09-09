-- Remove the index comment. Reverts before 20260908000000 drops the index
-- itself, so the target still exists at this point.

COMMENT ON INDEX idx_batches_unfrozen_sweep IS NULL;
