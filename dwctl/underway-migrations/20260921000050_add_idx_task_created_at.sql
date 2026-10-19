-- no-transaction
-- Retention index: the task-retention daemon and the manual purge script delete
-- expired tasks oldest-first in bounded batches (ORDER BY created_at LIMIT n
-- with a created_at upper bound). Without this the range scan degrades to a
-- sort of the whole task history on every batch.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_task_created_at
    ON underway.task (created_at);
