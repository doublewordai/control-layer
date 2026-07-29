-- Add the distinct background inference tier to the usage ledger and its
-- per-batch read model. Install the broader checks before removing the old
-- ones so every committed application write remains constrained throughout
-- the migration.
ALTER TABLE credits_transactions
    ADD CONSTRAINT credits_transactions_service_tier_check_v2
    CHECK (service_tier IN ('realtime', 'flex', 'async', 'batch', 'background')) NOT VALID;

ALTER TABLE credits_transactions
    DROP CONSTRAINT credits_transactions_service_tier_check;

ALTER TABLE credits_transactions
    RENAME CONSTRAINT credits_transactions_service_tier_check_v2
    TO credits_transactions_service_tier_check;

-- Deliberately leave the replacement NOT VALID: PostgreSQL still enforces it
-- for every new or updated row, while the previously validated narrower check
-- proves all existing rows are in this broader set. Avoiding VALIDATE here
-- prevents a full scan of the large, hot ledger.

ALTER TABLE batch_aggregates
    ADD CONSTRAINT batch_aggregates_service_tier_check_v2
    CHECK (service_tier IN ('async', 'batch', 'background')) NOT VALID;

ALTER TABLE batch_aggregates
    DROP CONSTRAINT batch_aggregates_service_tier_check;

ALTER TABLE batch_aggregates
    RENAME CONSTRAINT batch_aggregates_service_tier_check_v2
    TO batch_aggregates_service_tier_check;

ALTER TABLE batch_aggregates
    VALIDATE CONSTRAINT batch_aggregates_service_tier_check;
