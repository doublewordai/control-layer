DROP INDEX IF EXISTS idx_requests_flex_created;
DROP INDEX IF EXISTS idx_requests_active_first_flex;
ALTER TABLE requests DROP CONSTRAINT IF EXISTS requests_service_tier_check;
ALTER TABLE requests DROP COLUMN IF EXISTS service_tier;
