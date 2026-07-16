-- no-transaction
--
-- Migration 121 (COR-524 follow-up / "Usage E"): partial index backing the responses view's
-- per-request cost lookup (credits_transactions by fusillade_request_id, added in migration
-- 120). The reader is
--   SELECT DISTINCT ON (fusillade_request_id) amount FROM credits_transactions
--   WHERE fusillade_request_id = ANY($1) AND transaction_type = 'usage'
--   ORDER BY fusillade_request_id, seq DESC
-- so the index is keyed (fusillade_request_id, seq DESC) to satisfy the DISTINCT ON's
-- per-group latest-by-seq without a sort. Partial on `fusillade_request_id IS NOT NULL`
-- (only usage rows ever carry a request id, so this is also usage-only) keeps it small.
-- Built CONCURRENTLY so it can't block the batcher's continuous inserts, hence
-- `-- no-transaction` (same pattern as migrations 115 / 118). Split from 120 because CREATE
-- INDEX CONCURRENTLY cannot share a migration with a transaction-wrapped statement.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_credits_tx_fusillade_request_id
    ON credits_transactions (fusillade_request_id, seq DESC)
    WHERE fusillade_request_id IS NOT NULL;
