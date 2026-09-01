-- Make created_by non-nullable for per-user fair scheduling (COR-227).
-- No NULL values exist in production; this just enforces the constraint.

ALTER TABLE batches ALTER COLUMN created_by SET DEFAULT '';
UPDATE batches SET created_by = '' WHERE created_by IS NULL;
ALTER TABLE batches ALTER COLUMN created_by SET NOT NULL;

-- The existing partial index excluded NULLs; replace with a full index.
DROP INDEX IF EXISTS idx_batches_created_by;
CREATE INDEX idx_batches_created_by ON batches(created_by);
