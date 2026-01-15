-- Add sort_order column to deployed_model_components for explicit priority ordering
-- In priority mode, onwards uses insertion order (not weight values), so we need
-- an explicit sort_order column to persist the intended priority order.

ALTER TABLE deployed_model_components
ADD COLUMN sort_order INTEGER NOT NULL DEFAULT 0;

-- Populate sort_order based on current weight ordering (higher weight = higher priority = lower sort_order)
-- This preserves existing behavior where weight DESC determined priority
WITH ranked AS (
    SELECT id, ROW_NUMBER() OVER (PARTITION BY composite_model_id ORDER BY weight DESC, created_at) - 1 AS new_order
    FROM deployed_model_components
)
UPDATE deployed_model_components dmc
SET sort_order = ranked.new_order
FROM ranked
WHERE dmc.id = ranked.id;

-- Add index for efficient ordering queries
CREATE INDEX idx_deployed_model_components_sort_order ON deployed_model_components(composite_model_id, sort_order);
