-- Track short-lived capacity reservations to prevent batch creation race conditions.
-- Expired rows are ignored by queries (cleanup can be added later if needed).

CREATE TABLE batch_capacity_reservations (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    model_id UUID NOT NULL REFERENCES deployed_models(id) ON DELETE CASCADE,
    completion_window TEXT NOT NULL,
    reserved_requests BIGINT NOT NULL CHECK (reserved_requests > 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL,
    released_at TIMESTAMPTZ DEFAULT NULL
);

-- Fast lookup for summing active reservations per model + window.
-- Partial index keeps it small and hot.
CREATE INDEX idx_batch_capacity_reservations_active
    ON batch_capacity_reservations (model_id, completion_window, expires_at)
    WHERE released_at IS NULL;

-- Optional: direct lookup by model/window without expiry filter
CREATE INDEX idx_batch_capacity_reservations_model_window
    ON batch_capacity_reservations (model_id, completion_window);