-- no-transaction
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_task_queue_state ON underway.task (task_queue_name, state);
