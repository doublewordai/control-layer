-- Recompute user_model_usage to include realtime requests.
-- Previously the aggregation only included batch requests (fusillade_batch_id IS NOT NULL).
-- Truncate the summary table and reset the cursor so the next refresh re-aggregates all rows.

TRUNCATE user_model_usage;
UPDATE user_model_usage_cursor SET last_processed_id = 0, updated_at = NOW() WHERE id = TRUE;
