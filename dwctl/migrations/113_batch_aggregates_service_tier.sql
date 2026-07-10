-- Migration 113 (COR-514): denormalize service_tier onto batch_aggregates.
--
-- The transactions list reads a service tier per batch. Today it derives that from
-- a per-batch LATERAL into http_analytics (backed by idx_analytics_fusillade_batch_id).
-- batch_aggregates is already the per-batch read model the UI reads, and it is small
-- (one row per batch), so carrying the tier here — rather than adding an index to the
-- huge credits_transactions ledger — is the cheaper way to drop that http_analytics
-- join.
--
-- For an aggregated batch the tier is `async` (1h SLA) or `batch` (24h SLA); it is set
-- at fold time by the analytics batcher and backfilled by
-- scripts/backfill_batch_aggregates_denorm.sh.
ALTER TABLE batch_aggregates
    ADD COLUMN service_tier TEXT;
