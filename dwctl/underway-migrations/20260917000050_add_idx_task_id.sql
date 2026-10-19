-- no-transaction
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_task_id ON underway.task (id);
