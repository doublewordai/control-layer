-- Remove the batch terminal timestamp trigger
-- This trigger was firing on EVERY request state update and doing expensive COUNT(*) queries
-- on all batch requests, causing severe performance issues during batch cancellation
-- (e.g., cancelling 10,000 requests = 10,000 trigger fires = 10,000 COUNT queries)
--
-- Terminal timestamps (finalizing_at, completed_at, failed_at, cancelled_at) will now be
-- computed lazily when needed rather than maintained eagerly via trigger.

DROP TRIGGER IF EXISTS update_batch_terminal_timestamps_trigger ON requests;
DROP FUNCTION IF EXISTS update_batch_terminal_timestamps();
