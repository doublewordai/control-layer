-- Partial index supporting the first-payment-match eligibility check.
--
-- The promotion (see credits.first_payment_match_up_to) asks "has this user ever
-- made a purchase before this one?" via:
--   SELECT NOT EXISTS (
--     SELECT 1 FROM credits_transactions
--     WHERE user_id = $1 AND transaction_type = 'purchase' AND source_id <> $2
--   )
--
-- credits_transactions is dominated by 'usage' rows (one per request/batch), and
-- the existing indexes are on (user_id, created_at) and (transaction_type) alone,
-- neither of which isolates a user's purchases. Without help, the EXISTS for a
-- user with lots of usage but no purchase yet (the exact case the promo fires on)
-- would wade through many rows. Purchases are sparse, so a partial index keyed on
-- user_id and filtered to purchases stays tiny and turns the check into a direct
-- lookup regardless of usage volume.
CREATE INDEX idx_credits_transactions_user_purchases
    ON credits_transactions (user_id)
    WHERE transaction_type = 'purchase';
