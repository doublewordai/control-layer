-- Sweep-candidate index for the batch archive (phase 3): the sweeper and the
-- historical backfill worker both select "frozen, live-or-split, not deleted"
-- batches ordered by created_at (both movers run OLDEST-first in production;
-- the same index scans in either direction if a caller ever chooses
-- newest-first). 'split' must be in the predicate: a re-frozen split batch
-- (retry of an archived batch that has resumed to terminal) is re-archived
-- through the same candidate listing, and a partial index only serves a
-- query whose predicate it implies. Without this index every tick sorts the
-- full batches table.
--
-- Size note: until the historical drain completes, every frozen live batch
-- matches (~800k rows); after the drain it shrinks to the small
-- just-terminalized window and stays there. Plain CREATE INDEX (migrations
-- run in a transaction): one scan of batches at boot, seconds — same profile
-- as the 20260716000001 index rebuild measured on staging (~4s, queued-not-
-- dropped writers).
CREATE INDEX idx_batches_archivable
    ON batches (created_at, id)
    WHERE location IN ('live', 'split') AND counts_frozen_at IS NOT NULL AND deleted_at IS NULL;

COMMENT ON INDEX idx_batches_archivable IS
    'Serves list_archivable_batches (archive sweeper + oldest-first '
    'backfill). Predicate must stay implied by that query''s WHERE.';
