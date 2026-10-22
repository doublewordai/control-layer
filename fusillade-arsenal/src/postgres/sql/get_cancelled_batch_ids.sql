SELECT id
FROM batches
WHERE id = ANY($1)
  AND cancelling_at IS NOT NULL
  AND deleted_at IS NULL
