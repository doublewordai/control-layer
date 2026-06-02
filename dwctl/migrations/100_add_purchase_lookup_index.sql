-- no-transaction
-- Partial index supporting the first-payment-match eligibility check.
--
-- The promotion (see credits.first_payment_match_up_to) asks, for a given
-- purchase, "is there an earlier purchase by this user?" via:
--   SELECT NOT EXISTS (
--     SELECT 1 FROM credits_transactions
--     WHERE user_id = $1 AND transaction_type = 'purchase' AND seq < $2
--   )
--
-- credits_transactions is dominated by 'usage' rows (one per request/batch), and
-- the existing indexes are on (user_id, created_at) and (transaction_type) alone,
-- neither of which isolates a user's purchases. Purchases are sparse, so a partial
-- index keyed on (user_id, seq) and filtered to purchases stays tiny and turns the
-- check into a direct lookup regardless of usage volume.
--
-- Built CONCURRENTLY (hence `-- no-transaction`): credits_transactions is write-hot,
-- and a plain CREATE INDEX would hold a lock that blocks credit writes for the
-- duration of the build.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_credits_transactions_user_purchases
    ON credits_transactions (user_id, seq)
    WHERE transaction_type = 'purchase';
