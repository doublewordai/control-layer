-- no-transaction
-- Add effective_batch_sla to track the actual SLA tier a batch request was charged at.
--
-- When a request submitted at a higher SLA (e.g. "1h") completes after that window,
-- billing falls through to the next tier (e.g. "24h"). If it completes after all
-- configured windows, it's free. This column records which tier was actually applied.
--
-- Values: the completion_window string of the tariff used (e.g. "1h", "24h"),
--         "free" if the request exceeded all configured windows,
--         purpose name (e.g. "batch") if a generic tariff was used,
--         "realtime" if pricing fell back to the generic realtime tariff,
--         or empty string for non-batch requests / when no tariff matched.
--
-- DEFAULT '' means non-batch rows are instantly correct without a backfill.
-- Only batch rows (batch_sla <> '') need updating.
ALTER TABLE http_analytics ADD COLUMN IF NOT EXISTS effective_batch_sla TEXT DEFAULT '';

-- Backfill batch rows only, in ordered batches for linear progress.
-- Prior to this migration there was no waterfall logic — every batch
-- request was charged at the submitted SLA tier, so effective_batch_sla
-- equals batch_sla for all historical batch rows.
--
-- Each iteration peeks at the next 10 000 batch-row IDs to find the
-- upper bound, then updates that ID range. Progress advances by ID so
-- the scan never revisits earlier rows. Each iteration auto-commits
-- independently (no-transaction mode).
DO $$
DECLARE
  _last_id bigint := 0;
  _batch_max bigint;
BEGIN
  LOOP
    SELECT MAX(id) INTO _batch_max
      FROM (
        SELECT id FROM http_analytics
         WHERE batch_sla <> ''
           AND id > _last_id
         ORDER BY id
         LIMIT 10000
      ) sub;

    EXIT WHEN _batch_max IS NULL;

    UPDATE http_analytics
       SET effective_batch_sla = batch_sla
     WHERE batch_sla <> ''
       AND id > _last_id
       AND id <= _batch_max;

    _last_id := _batch_max;
    PERFORM pg_sleep(0.1);
  END LOOP;
END
$$;
