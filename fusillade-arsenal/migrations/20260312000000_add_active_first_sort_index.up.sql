-- Expression index for "active first" batch sorting.
-- Lets Postgres walk the index in sorted order and stop at LIMIT,
-- avoiding a full-table sort when active_first=true.
CREATE INDEX IF NOT EXISTS idx_batches_active_first_sort
ON batches (
    (CASE WHEN completed_at IS NULL AND failed_at IS NULL
          AND cancelled_at IS NULL AND cancelling_at IS NULL
     THEN 0 ELSE 1 END),
    created_at DESC,
    id DESC
) WHERE deleted_at IS NULL;
