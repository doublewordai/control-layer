-- no-transaction
-- Retention preserves pending/in_progress tasks even after their TTL expires.
-- Exclude those rows so an oldest-first sweep does not repeatedly scan old live
-- task history before finding terminal rows (or proving none are eligible).
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_task_terminal_created_at
    ON underway.task (created_at)
    WHERE state IN ('succeeded', 'failed');
