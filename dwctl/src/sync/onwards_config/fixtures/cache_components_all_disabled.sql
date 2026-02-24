-- Disable all composite components for composite-priority.
UPDATE deployed_model_components
SET enabled = FALSE
WHERE composite_model_id = '50000000-0000-0000-0000-000000000001';
