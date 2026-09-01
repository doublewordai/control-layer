-- no-transaction
--
-- Migration 116 (COR-514 follow-up): constrain credits_transactions.service_tier
-- to the enumerated tiers.
--
-- service_tier (added NULLable in migration 112) is written once at INSERT by the
-- analytics batcher's compute_service_tier and backfilled for history by
-- scripts/backfill_credits_denorm.sh. Both only ever produce one of four literals:
--   realtime | flex | async | batch
-- (verified on the staging DB: 0 rows outside that set, 0 empty strings). NULL is
-- retained — non-usage rows (grants/purchases) never carry a tier, and historical usage
-- rows are NULL until the backfill runs (go-forward usage rows are written WITH a tier at
-- INSERT by the batcher). `IN (...)` passes NULL by SQL three-valued logic, so no explicit
-- `IS NULL` disjunct is needed.
--
-- credits_transactions is large (~49M rows) and hot (the batcher inserts
-- continuously). A plain `ADD CONSTRAINT ... CHECK` would hold ACCESS EXCLUSIVE
-- for a full validation scan and stall those writes, so we split it (hence
-- `-- no-transaction`, so each statement auto-commits into its own transaction):
--   1. ADD ... NOT VALID  — brief ACCESS EXCLUSIVE, enforces the check on all new
--      writes immediately, does NOT scan existing rows.
--   2. VALIDATE CONSTRAINT — scans existing rows under SHARE UPDATE EXCLUSIVE,
--      which does not block concurrent INSERT/UPDATE.
ALTER TABLE credits_transactions
    ADD CONSTRAINT credits_transactions_service_tier_check
    CHECK (service_tier IN ('realtime', 'flex', 'async', 'batch')) NOT VALID;

ALTER TABLE credits_transactions
    VALIDATE CONSTRAINT credits_transactions_service_tier_check;
