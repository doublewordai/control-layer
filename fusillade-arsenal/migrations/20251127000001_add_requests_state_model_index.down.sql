-- Drop the composite indexes for per-model capacity queries
DROP INDEX IF EXISTS idx_requests_model_in_progress;
DROP INDEX IF EXISTS idx_requests_state_model;
