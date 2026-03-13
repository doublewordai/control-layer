-- no-transaction
-- Add effective_batch_sla to track the SLA window a batch request was charged at
-- after waterfall resolution.
--
-- When a request submitted at a higher SLA (e.g. "1h") completes after that window,
-- billing falls through to the next tier (e.g. "24h"). If it completes after all
-- configured windows, it's free. This column records the effective window.
--
-- Values: completion_window string (e.g. "1h", "24h"),
--         "free" if the request exceeded all configured windows,
--         or "" for non-batch requests.
--
-- NOT NULL DEFAULT '' means non-batch rows are instantly correct without a backfill.
-- Only batch rows (batch_sla <> '') need updating.
ALTER TABLE http_analytics ADD COLUMN IF NOT EXISTS effective_batch_sla TEXT NOT NULL DEFAULT '';

-- Backfill batch rows only, in ordered batches for linear progress.
-- Prior to this migration there was no waterfall logic — every batch
-- request was charged at the submitted SLA tier, so effective_batch_sla
-- equals batch_sla for all historical batch rows.
-- Each iteration auto-commits independently (no-transaction mode).
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
           AND effective_batch_sla = ''
           AND id > _last_id
         ORDER BY id
         LIMIT 10000
      ) sub;

    EXIT WHEN _batch_max IS NULL;

    UPDATE http_analytics
       SET effective_batch_sla = batch_sla
     WHERE batch_sla <> ''
       AND effective_batch_sla = ''
       AND id > _last_id
       AND id <= _batch_max;

    _last_id := _batch_max;
  END LOOP;
END
$$;
