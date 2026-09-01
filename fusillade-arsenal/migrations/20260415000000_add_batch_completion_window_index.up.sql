-- Index to support filtering batches by completion_window (e.g., async page filtering for '1h').
-- Partial index excludes soft-deleted batches since all list queries filter on deleted_at IS NULL.
-- Includes created_by for ownership filtering in the same scan.
CREATE INDEX IF NOT EXISTS idx_batches_completion_window
ON batches (completion_window, created_by)
WHERE deleted_at IS NULL;
