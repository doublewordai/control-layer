-- no-transaction
-- Ordered claim index for Underway 0.2.0's dequeue. The claim orders by
-- (priority DESC, created_at, id) and takes one row, so an index in that order
-- restricted to live rows lets the planner walk it and stop at the first
-- claimable task instead of collecting every live row and sorting it. A
-- parameterized (generic) plan cannot prove the partial predicate and keeps
-- using idx_task_queue_state, which is why that index stays.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_task_claim
    ON underway.task (task_queue_name, priority DESC, created_at, id)
    WHERE state IN ('pending', 'in_progress');
