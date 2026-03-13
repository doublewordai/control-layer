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
ALTER TABLE http_analytics ADD COLUMN IF NOT EXISTS effective_batch_sla TEXT NOT NULL DEFAULT '';

-- Backfill: before this migration, all batch requests were charged at the submitted SLA,
-- so effective_batch_sla = batch_sla for existing batch rows.
-- Simple UPDATE is fine here: the ALTER TABLE above already committed (no-transaction mode),
-- so this UPDATE runs in its own short transaction without holding the DDL lock.
UPDATE http_analytics SET effective_batch_sla = batch_sla WHERE batch_sla != '' AND effective_batch_sla = '';
