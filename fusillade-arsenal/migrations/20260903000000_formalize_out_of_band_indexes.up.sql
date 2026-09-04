-- Formalize two indexes that exist in production but in no migration (COR-653).
--
-- Both were created out-of-band and are live, actively-used schema that a fresh
-- deploy would not reproduce. Same class of drift as
-- 20260630000000_formalize_user_sort_indexes, which reconciled two indexes
-- created by the batchless-responses backfill script; this migration does the
-- same for two more found during the COR-537 index audit.
--
-- Definitions below are transcribed verbatim from production `pg_get_indexdef`
-- (primary, 2026-09-03), so the `IF NOT EXISTS` statements are no-ops there and
-- on any copy-on-write branch taken from it.
--
-- 1. idx_batches_claimable_expiration_with_id
--
--    The tightened successor to idx_batches_active_expiration_with_id
--    (20260506000000). That index filters only `cancelling_at IS NULL`; this one
--    adds the four remaining terminal-state predicates, so it indexes just the
--    genuinely claimable batches. The size difference is dramatic and explains
--    why the older index is dropped in the migration that follows this one:
--
--      idx_batches_active_expiration_with_id     90 MB        0 scans
--      idx_batches_claimable_expiration_with_id  320 kB  65,223 scans
--
--    (production primary, 35-day stats window)
--
-- 2. idx_requests_completed_at
--
--    93,161 scans in the same window, alongside idx_requests_completed_trailing
--    (3.2 GB, 150,245 scans) rather than instead of it. The overlap between the
--    two is still unexplained and is tracked as part of the "uncertain indexes"
--    review (COR-657). Formalizing it here is deliberate and independent of that
--    review: if it is later dropped, that should be an explicit migration rather
--    than a silent divergence between production and the migration set.
--
-- Production deploy: both already exist, so nothing runs. For any deployment
-- where they do not, build them CONCURRENTLY first so this migration stays a
-- no-op:
--
--   CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_batches_claimable_expiration_with_id
--     ON batches (expires_at) INCLUDE (id)
--     WHERE cancelling_at IS NULL AND deleted_at IS NULL AND completed_at IS NULL
--       AND failed_at IS NULL AND cancelled_at IS NULL;
--
--   CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_requests_completed_at
--     ON requests (completed_at) WHERE completed_at IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_batches_claimable_expiration_with_id
  ON batches (expires_at)
  INCLUDE (id)
  WHERE cancelling_at IS NULL
    AND deleted_at IS NULL
    AND completed_at IS NULL
    AND failed_at IS NULL
    AND cancelled_at IS NULL;

COMMENT ON INDEX idx_batches_claimable_expiration_with_id IS
'Claimable-batch expiration index with id INCLUDEd for index-only joins on batch id. Supersedes idx_batches_active_expiration_with_id, whose looser predicate indexed every non-cancelling batch including terminal ones.';

CREATE INDEX IF NOT EXISTS idx_requests_completed_at
  ON requests (completed_at)
  WHERE completed_at IS NOT NULL;

COMMENT ON INDEX idx_requests_completed_at IS
'Formalized from out-of-band production schema (COR-653). Overlaps idx_requests_completed_trailing; the split of traffic between them is under review in COR-657.';
