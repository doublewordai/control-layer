-- Migration 120 (COR-524 follow-up / "Usage E"): denormalize fusillade_request_id onto
-- credits_transactions so the responses view (GET /admin/api/v1/batches/requests[/{id}])
-- can read per-request COST off the ledger by fusillade_request_id, durably — instead of
-- joining http_analytics (source_id -> http_analytics.id -> fusillade_request_id), which
-- routes through a table that COR-509 partitions + prunes on retention. (Per-request token
-- counts stay best-effort on http_analytics and empty-state after retention; response time
-- already comes from the durable fusillade request row.)
--
-- The batcher sets it at INSERT (it already has record.raw.fusillade_request_id in memory);
-- scripts/backfill_credits_fusillade_request_id.sh fills history from http_analytics while
-- that data still exists. NULL for realtime (non-fusillade) usage and pre-backfill rows.
--
-- ADD COLUMN is nullable / no default → metadata-only (no table rewrite). The lookup index is
-- built CONCURRENTLY in the next migration (121), which must run outside a transaction.
ALTER TABLE credits_transactions
    ADD COLUMN fusillade_request_id UUID;
