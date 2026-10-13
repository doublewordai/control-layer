-- no-transaction
-- Customer batch pages filtered to a sparse terminal status.
--
-- The dashboard's failed-batch filter lists a user's newest batches first:
--
--   WHERE deleted_at IS NULL
--     AND failed_at IS NOT NULL AND completed_at IS NULL
--     AND (completion_window = ANY(...) OR service_tier = ANY(...))
--   ORDER BY created_at DESC, id DESC LIMIT n
--
-- Failed batches are rare (about 1% of history), so with only
-- idx_batches_created_at matching the ORDER BY, the planner walks `batches`
-- newest-first and discards rows one by one until it has a page. In production
-- on 2026-10-12 this removed 1.3M rows and ran 15.1s, past the 15s PAGE_BUDGET,
-- so GET /ai/v1/batches returned 500. This index holds only failed, undeleted
-- batches, so the same page is a short scan of that subset.
--
-- The predicate is exactly the `status=failed` predicate `push_arm` emits
-- (batch_list.rs), so a generic prepared plan can prove it. If that mapping
-- changes, change this predicate with it.
--
-- Keep this file to one statement: concurrent builds cannot run in a
-- transaction. A failed build can leave an INVALID index that IF NOT EXISTS
-- skips; the following reindex and validate migrations repair and check it.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_batches_failed_created_at_id
ON batches (created_at DESC, id DESC)
WHERE failed_at IS NOT NULL AND completed_at IS NULL AND deleted_at IS NULL;
