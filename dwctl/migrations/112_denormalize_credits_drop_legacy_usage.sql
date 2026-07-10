-- Migration 112: contract the A-rollout (drop legacy usage tables) + expand B
-- (denormalize the three presentational fields onto the credits ledger).
--
-- Gated on PR #1248 (COR-506 Deploy 2 cutover) being fully rolled out in prod,
-- which is satisfied: every pod now serves /usage from user_model_usage_daily and
-- no code reads user_model_usage. Both halves are fast DDL and safe in one txn.

-- === COR-513: drop the legacy all-time usage accumulator ===
-- Superseded by user_model_usage_daily (migration 110). Deploy 2 removed every
-- reader + the inline refresh_user_model_usage, so nothing writes or reads these.
-- The backfill-progress table is an ops artifact left by the daily backfill script.
DROP TABLE IF EXISTS user_model_usage;
DROP TABLE IF EXISTS user_model_usage_cursor;
DROP TABLE IF EXISTS user_model_usage_daily_backfill_progress;

-- === COR-507: denormalize the service tier onto the ledger ===
-- credits_transactions (migration 023) is the immutable source of truth for
-- billing; the transactions list only still touched http_analytics to label each
-- row's service tier (via an unbounded id::text join + a per-batch LATERAL).
-- Carrying a single computed `service_tier` on the ledger lets the transactions
-- query drop both http_analytics joins (COR-514).
--
-- service_tier ∈ {realtime, flex, async, batch}, computed in memory by the
-- analytics batcher from (fusillade_batch_id, completion_window):
--   realtime = no batch id, no SLA;  flex = 1h SLA, no batch id;
--   async    = 1h SLA + batch id;    batch = 24h SLA + batch id.
-- Set at INSERT going forward; backfilled for history by
-- scripts/backfill_credits_denorm.sh. No immutability-trigger change is needed:
-- the trigger only guards its enumerated columns, and this is written once at
-- INSERT (same pattern as fusillade_batch_id / is_aggregated, migration 050).
ALTER TABLE credits_transactions
    ADD COLUMN service_tier TEXT;
