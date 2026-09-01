-- Composite indexes with service_tier as a trailing column.
--
-- Supports all service tier query patterns with just two indexes:
--   - Specific tier:  WHERE service_tier = 'flex'
--   - Multiple tiers: WHERE service_tier IN ('flex', 'priority')
--   - All tiers:      no service_tier filter (full index scan)
--
-- service_tier as a trailing column lets Postgres apply it as an Index Cond
-- filter while preserving the sort order from the leading columns. Adding
-- new tiers in the future requires zero index changes.
--
-- Two indexes cover both sort orderings used by list_requests:
--   1. active_first=false → created_at DESC, id DESC
--   2. active_first=true  → CASE state ... ASC, created_at DESC, id DESC
--
-- These supersede the flex-specific partial indexes which are dropped below.
--
-- NOTE: On large tables, create the new indexes CONCURRENTLY before applying
-- the migration so the IF NOT EXISTS makes it a no-op:
--
--   CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_requests_created_tier
--   ON requests (created_at DESC, id DESC, service_tier);
--
--   CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_requests_active_first_tier
--   ON requests (
--     (CASE state WHEN 'processing' THEN 0 WHEN 'claimed' THEN 1 WHEN 'pending' THEN 2 ELSE 3 END),
--     created_at DESC, id DESC, service_tier
--   );

CREATE INDEX IF NOT EXISTS idx_requests_created_tier ON requests
  USING btree (created_at DESC, id DESC, service_tier);

CREATE INDEX IF NOT EXISTS idx_requests_active_first_tier ON requests
  USING btree (
    (CASE state WHEN 'processing' THEN 0 WHEN 'claimed' THEN 1 WHEN 'pending' THEN 2 ELSE 3 END),
    created_at DESC, id DESC, service_tier
  );

-- Drop flex-specific partial indexes now superseded by the composite indexes above.
DROP INDEX IF EXISTS idx_requests_active_first_flex;
DROP INDEX IF EXISTS idx_requests_flex_created;
