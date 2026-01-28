-- Remove the balance threshold trigger from credits_transactions
--
-- The previous row-level trigger ran a SUM query for EVERY row inserted,
-- causing batch inserts of 100 rows to execute 100 separate SUM queries.
-- This was taking ~2.5 seconds per batch and blocking cost tracking.
--
-- Balance threshold notifications are now handled in application code
-- (request_logging/batcher.rs) which:
-- 1. Batch inserts credit transactions
-- 2. Queries balances once after insert (with probabilistic checkpoint refresh)
-- 3. Sends pg_notify for any user with balance <= 0
--
-- This reduces ~100 SUM queries to 1 bulk balance query per batch.

-- Drop the trigger
DROP TRIGGER IF EXISTS credits_balance_threshold_notify ON credits_transactions;

-- Drop the function (no longer needed)
DROP FUNCTION IF EXISTS notify_on_balance_threshold_crossing();
