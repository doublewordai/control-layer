-- Migration: Remove model column from batch_aggregates
-- Batches can contain multiple models, so storing a single model value
-- was incorrect. The batch UUID is sufficient for identification.

ALTER TABLE batch_aggregates DROP COLUMN IF EXISTS model;
