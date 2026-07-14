-- no-transaction
--
-- Migration 120 (COR-524 follow-up / "Usage E"): partial index backing the responses view's
-- per-request cost lookup (credits_transactions by fusillade_request_id, added in migration
-- 119). Partial (fusillade requests only — realtime usage carries no request id) and built
-- CONCURRENTLY so it can't block the batcher's continuous inserts, hence `-- no-transaction`
-- (same pattern as migrations 115 / 118). Split from 119 because CREATE INDEX CONCURRENTLY
-- cannot share a migration with a transaction-wrapped statement.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_credits_tx_fusillade_request_id
    ON credits_transactions (fusillade_request_id)
    WHERE fusillade_request_id IS NOT NULL;
