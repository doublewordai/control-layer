-- Soft-delete regular-public deployment.
UPDATE deployed_models
SET deleted = TRUE
WHERE id = '40000000-0000-0000-0000-000000000001';
