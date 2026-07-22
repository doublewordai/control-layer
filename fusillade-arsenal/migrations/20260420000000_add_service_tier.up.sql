-- Add column: metadata-only operation in PG11+, instant on any table size.
-- NULL = batch tier, non-null values = async tiers (flex, etc.).
--
-- Pre-deploy sequence (run manually before deploying):
--   1. Run this ADD COLUMN + ADD CONSTRAINT (~60ms)
--   2. CREATE INDEX CONCURRENTLY x2 (~10min each, no locks)
--   3. Run backfill script (~22min, no locks)
--   4. Deploy — migration is a no-op (all IF NOT EXISTS)
ALTER TABLE requests ADD COLUMN IF NOT EXISTS service_tier text DEFAULT NULL;

-- NOT VALID skips the full-table validation scan.
-- New inserts are checked immediately; existing rows are validated after
-- the backfill via a separate VALIDATE CONSTRAINT statement.
DO $$ BEGIN
  ALTER TABLE requests ADD CONSTRAINT requests_service_tier_check
    CHECK (service_tier IN ('auto', 'default', 'flex', 'priority')) NOT VALID;
EXCEPTION WHEN duplicate_object THEN NULL;
END $$;

-- Partial index for flex-tier requests with active_first=true ordering.
--
-- NOTE: CREATE INDEX without CONCURRENTLY takes an ACCESS EXCLUSIVE lock for
-- the duration of the build. On large tables create this CONCURRENTLY ahead
-- of applying the migration; the IF NOT EXISTS below makes the migration a
-- no-op in that case:
--
--   CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_requests_active_first_flex
--   ON requests (
--     (CASE state WHEN 'processing' THEN 0 WHEN 'claimed' THEN 1 WHEN 'pending' THEN 2 ELSE 3 END),
--     created_at DESC, id DESC
--   ) WHERE service_tier = 'flex' AND batch_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_requests_active_first_flex ON requests
  USING btree (
    (CASE state WHEN 'processing' THEN 0 WHEN 'claimed' THEN 1 WHEN 'pending' THEN 2 ELSE 3 END),
    created_at DESC, id DESC
  ) WHERE service_tier = 'flex' AND batch_id IS NOT NULL;

-- Partial index for flex-tier requests with active_first=false ordering
-- (created_at DESC, id DESC).
--
--   CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_requests_flex_created
--   ON requests (created_at DESC, id DESC)
--   WHERE service_tier = 'flex' AND batch_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_requests_flex_created ON requests
  USING btree (created_at DESC, id DESC)
  WHERE service_tier = 'flex' AND batch_id IS NOT NULL;
