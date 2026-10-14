-- no-transaction
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_batch_capacity_reservations_released
    ON batch_capacity_reservations (model_id, released_at)
    INCLUDE (completion_window, reserved_requests)
    WHERE released_at IS NOT NULL;
