-- no-transaction
-- A hand-built (task_queue_name, created_at) WHERE state = 'pending' index has
-- existed on some installations. No release created or used it: the claim
-- cannot use a pending-only index (its stalled-task arm reads in_progress rows)
-- and idx_task_claim now covers the ordered pending lookup, so it is only write
-- amplification. Safe to drop under every release.
DROP INDEX CONCURRENTLY IF EXISTS underway.idx_task_pending_by_queue_created;
