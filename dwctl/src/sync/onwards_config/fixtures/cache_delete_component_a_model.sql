-- Soft-delete component-a model. Composite should retain only component-b.
UPDATE deployed_models
SET deleted = TRUE
WHERE id = '40000000-0000-0000-0000-000000000005';
