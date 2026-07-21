CREATE INDEX IF NOT EXISTS idx_batches_active
ON batches (id)
WHERE cancelling_at IS NULL
  AND deleted_at IS NULL
  AND completed_at IS NULL
  AND failed_at IS NULL
  AND cancelled_at IS NULL;
