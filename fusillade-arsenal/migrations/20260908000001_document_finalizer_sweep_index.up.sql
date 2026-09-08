-- Comment for the index created by 20260908000000 (COR-649).
--
-- Split out because that migration builds CONCURRENTLY and therefore runs
-- outside a transaction, where Postgres permits only a single statement. Same
-- split as dwctl migrations 120 / 121.

COMMENT ON INDEX idx_batches_unfrozen_sweep IS
'Candidate lookup for the three batch-finalizer arms, all of which filter on counts_frozen_at IS NULL AND deleted_at IS NULL. Keyed on cancelled_at so finalize_cancelled_batches also gets its ORDER BY from the index. Holds only unfinalized batches, so it stays tiny regardless of history.';
