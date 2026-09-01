-- Add composite index on (state, model) for efficient per-model capacity queries
-- This index supports queries that filter by both state and model, which is critical
-- for the per-model claim queries that check in-progress counts.
--
-- We use a partial index to only index non-terminal states, reducing index size.
CREATE INDEX idx_requests_state_model
ON requests(state, model)
WHERE state IN ('pending', 'claimed', 'processing');

-- Add a partial index optimized specifically for counting in-progress requests per model
-- This covers the pattern: WHERE model = ? AND state IN ('claimed', 'processing')
-- Note: idx_requests_model already exists from previous migration, this is a more specific index
CREATE INDEX idx_requests_model_in_progress
ON requests(model, state)
WHERE state IN ('claimed', 'processing');
