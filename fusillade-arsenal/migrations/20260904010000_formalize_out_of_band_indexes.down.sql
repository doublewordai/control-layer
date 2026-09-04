-- NOTE: both indexes exist in production and are in active use (65,223 and
-- 93,161 scans respectively over a 35-day window). Rolling this migration back
-- against production would drop live, actively-used indexes. The down migration
-- exists to keep the pair reversible on development and test databases.
DROP INDEX IF EXISTS idx_requests_completed_at;
DROP INDEX IF EXISTS idx_batches_claimable_expiration_with_id;
