-- Normalise composite-model component priority so each component has a unique,
-- dense sort_order within its composite.
--
-- Background: sort_order (migration 055) defaults to 0, so a composite could end
-- up with several components sharing sort_order = 0. Under the `priority`
-- load-balancing strategy, onwards iterates providers in sort_order and tries the
-- first one first; the sync query that feeds onwards orders only by sort_order
-- with no secondary key, so tied components resolve in an arbitrary order. The
-- result is that the component shown as "Primary" in the dashboard is not
-- guaranteed to be the one onwards actually tries first.
--
-- This renumbers every composite's components to 0..n-1, preserving the order
-- the dashboard already displayed (sort_order, then weight DESC, then created_at)
-- so no operator's intended priority is silently flipped. The API layer keeps the
-- invariant going forward (append-on-add, renumber-on-convert-to-priority,
-- move-and-reindex-on-reorder).
WITH ranked AS (
    SELECT
        id,
        ROW_NUMBER() OVER (
            PARTITION BY composite_model_id
            ORDER BY sort_order ASC, weight DESC, created_at ASC
        ) - 1 AS new_order
    FROM deployed_model_components
)
UPDATE deployed_model_components dmc
SET sort_order = ranked.new_order
FROM ranked
WHERE dmc.id = ranked.id
  AND dmc.sort_order IS DISTINCT FROM ranked.new_order;
