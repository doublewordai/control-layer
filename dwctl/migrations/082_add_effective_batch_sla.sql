-- no-transaction
-- Add effective_batch_sla to track the actual SLA tier a batch request was charged at.
--
-- When a request submitted at a higher SLA (e.g. "1h") completes after that window,
-- billing falls through to the next tier (e.g. "24h"). If it completes after all
-- configured windows, it's free. This column records which tier was actually applied.
--
-- Values: the completion_window string of the tariff used (e.g. "1h", "24h"),
--         "free" if the request exceeded all configured windows,
--         or empty string for non-batch requests / when no tariff matched.
--
-- Column is nullable so the ALTER is instant. Existing rows are backfilled
-- in non-blocking batches of 10 000 below: batch requests get batch_sla
-- as a reasonable default, non-batch requests get empty string.
ALTER TABLE http_analytics ADD COLUMN IF NOT EXISTS effective_batch_sla TEXT;

-- Backfill NULL rows in small batches to avoid long-running locks.
-- Prior to this migration there was no waterfall logic — every batch
-- request was charged at the submitted SLA tier, so effective_batch_sla
-- equals batch_sla for all historical rows. Non-batch requests get
-- empty string.
-- Each UPDATE touches at most 10 000 rows via a sub-select with LIMIT,
-- and the loop exits once no rows remain. Because we're in no-transaction
-- mode each iteration auto-commits independently.
DO $$
DECLARE
  _rows int;
BEGIN
  LOOP
    UPDATE http_analytics
       SET effective_batch_sla = CASE
             WHEN batch_sla <> '' THEN batch_sla
             ELSE ''
           END
     WHERE id IN (
       SELECT id FROM http_analytics
        WHERE effective_batch_sla IS NULL
        LIMIT 10000
     );
    GET DIAGNOSTICS _rows = ROW_COUNT;
    EXIT WHEN _rows = 0;
    -- Brief pause to let other transactions through
    PERFORM pg_sleep(0.1);
  END LOOP;
END
$$;
