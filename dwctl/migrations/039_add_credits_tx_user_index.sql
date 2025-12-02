-- Add composite index on credits_transactions for efficient user balance lookups
--
-- Optimizes the query pattern: DISTINCT ON (user_id) ORDER BY created_at DESC, id DESC
-- Allows direct lookup of latest transaction per user instead of scanning entire table.

CREATE INDEX IF NOT EXISTS idx_credits_tx_user_latest
ON credits_transactions(user_id, created_at DESC, id DESC);
