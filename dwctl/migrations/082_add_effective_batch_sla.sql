-- no-transaction
-- Add effective_batch_sla to track the actual SLA tier a batch request was charged at.
--
-- When a request submitted at a higher SLA (e.g. "1h") completes after that window,
-- billing falls through to the next tier (e.g. "24h"). If it completes after all
-- configured windows, it's free. This column records which tier was actually applied.
--
-- Values: the completion_window string of the tariff used (e.g. "1h", "24h"),
--         "free" if the request exceeded all configured windows,
--         or empty string for non-batch requests.
ALTER TABLE http_analytics ADD COLUMN effective_batch_sla TEXT NOT NULL DEFAULT '';

-- Backfill: before this migration, all batch requests were charged at the submitted SLA,
-- so effective_batch_sla = batch_sla for existing batch rows.
-- Batched to avoid long-running locks on large tables.
DO $$
DECLARE
    rows_updated INT;
BEGIN
    LOOP
        UPDATE http_analytics
        SET effective_batch_sla = batch_sla
        WHERE id IN (
            SELECT id FROM http_analytics
            WHERE batch_sla != '' AND effective_batch_sla = ''
            LIMIT 10000
        );
        GET DIAGNOSTICS rows_updated = ROW_COUNT;
        EXIT WHEN rows_updated = 0;
        PERFORM pg_sleep(0.1);
    END LOOP;
END $$;
