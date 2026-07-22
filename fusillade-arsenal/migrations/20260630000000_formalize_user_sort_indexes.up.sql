-- Formalize the two per-user batchless-request indexes into the migration set.
--
-- `idx_requests_user_active_sort` and `idx_requests_user_created_sort` back the
-- per-user response listing, and now also the COR-481 unverified upload-volume
-- count query. They were created out-of-band by
-- `scripts/backfill_responses_to_batchless.sql` (CONCURRENTLY) during the
-- batchless-responses rollout and were never added to a migration — so the
-- migration history drifts from production, where both indexes exist. This
-- migration reconciles that drift so fresh deploys get them too.
--
-- COR-481: the unverified upload-volume limit counts a user's recent flex
-- requests with `WHERE created_by = $1 AND created_at >= $2 AND service_tier =
-- 'flex'`; `idx_requests_user_created_sort` serves it as an index range scan
-- (seek created_by, range created_at). `service_tier = 'flex'` sits after the
-- created_at range column, so it's a residual filter rather than a scan bound,
-- but keeping it in the index lets the count stay index-only (no heap fetch).
--
-- A non-concurrent build holds ACCESS EXCLUSIVE on `requests` for its duration.
-- Deployments that already ran the backfill script (and any copy-on-write
-- database branches taken from them) already have these indexes, so the
-- `IF NOT EXISTS` statements below are a no-op there. For any other large
-- deployment, create them CONCURRENTLY before deploying so these become no-ops:
--
--   CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_requests_user_active_sort
--   ON requests (
--     created_by,
--     (CASE state WHEN 'processing' THEN 0 WHEN 'claimed' THEN 1 WHEN 'pending' THEN 2 ELSE 3 END),
--     created_at DESC, id DESC, service_tier
--   ) WHERE created_by IS NOT NULL;
--
--   CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_requests_user_created_sort
--   ON requests (created_by, created_at DESC, id DESC, service_tier)
--   WHERE created_by IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_requests_user_active_sort
ON requests (
    created_by,
    (CASE state WHEN 'processing' THEN 0 WHEN 'claimed' THEN 1 WHEN 'pending' THEN 2 ELSE 3 END),
    created_at DESC,
    id DESC,
    service_tier
) WHERE created_by IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_requests_user_created_sort
ON requests (
    created_by,
    created_at DESC,
    id DESC,
    service_tier
) WHERE created_by IS NOT NULL;
