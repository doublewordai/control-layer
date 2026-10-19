-- no-transaction
--
-- Candidate lookup for scheduled file-content expiry (expire_file_content).
--
-- The expiry sweep selects unexpired batch input files by upload age, oldest
-- first, in bounded chunks:
--
--   WHERE purpose = 'batch' AND deleted_at IS NULL
--     AND created_at <= statement_timestamp() - make_interval(days => $1)
--   ORDER BY created_at, id LIMIT $2 FOR UPDATE SKIP LOCKED
--
-- No index covers that ordering, so every sweep scanned and sorted the whole
-- files table, and the scan grew with the upload history. This index was
-- previously documented as optional for operators to build; building it here
-- makes the sweep a bounded range read on every installation.
--
-- The predicate is the query's static conditions, so generic plans can use it,
-- and a row leaves the index when its file is deleted or expired.
--
-- One statement only: CONCURRENTLY cannot run in a transaction. If an operator
-- already built this index from the earlier note, IF NOT EXISTS skips the
-- build and the following migrations reindex and validate it.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_files_content_expiry_due
ON files (created_at, id)
WHERE purpose = 'batch' AND deleted_at IS NULL;
