-- Add checkpoint-based balance calculation to remove advisory locking from credit transactions
-- This enables lock-free writes with O(1) amortized read complexity
--
-- Uses BIGSERIAL sequence numbers (not timestamps) for reliable ordering.
-- Timestamps can collide at microsecond precision under high concurrency,
-- but sequence numbers guarantee strict ordering.

-- 1. Add sequence column to credits_transactions for reliable ordering
ALTER TABLE credits_transactions ADD COLUMN seq BIGSERIAL;

COMMENT ON COLUMN credits_transactions.seq IS 'Auto-incrementing sequence number for reliable transaction ordering in checkpoint calculations';

-- 2. Create checkpoint table for caching running balances
CREATE TABLE user_balance_checkpoints (
    user_id UUID PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    checkpoint_seq BIGINT NOT NULL,
    balance DECIMAL(20, 9) NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

COMMENT ON TABLE user_balance_checkpoints IS 'Cached balance checkpoints for efficient balance calculation without locking';
COMMENT ON COLUMN user_balance_checkpoints.checkpoint_seq IS 'Sequence number of the last transaction included in the checkpoint balance';
COMMENT ON COLUMN user_balance_checkpoints.balance IS 'Aggregated balance of all transactions up to checkpoint_seq';

-- 3. Create covering index for efficient delta queries using seq
CREATE INDEX idx_credits_checkpoint_delta ON credits_transactions
    (user_id, seq DESC) INCLUDE (transaction_type, amount);

-- 4. Make balance_after nullable (we stop populating it for new transactions)
ALTER TABLE credits_transactions
    ALTER COLUMN balance_after DROP NOT NULL;

-- 5. Backfill checkpoints for all existing users with transactions
INSERT INTO user_balance_checkpoints (user_id, checkpoint_seq, balance)
SELECT
    user_id,
    MAX(seq) as checkpoint_seq,
    SUM(CASE WHEN transaction_type IN ('admin_grant', 'purchase') THEN amount ELSE -amount END) as balance
FROM credits_transactions
GROUP BY user_id
ON CONFLICT (user_id) DO NOTHING;

-- 6. Update the balance threshold notification trigger to use seq-based calculation
CREATE OR REPLACE FUNCTION notify_on_balance_threshold_crossing() RETURNS trigger AS $$
DECLARE
    old_balance DECIMAL(20, 9);
    new_balance DECIMAL(20, 9);
    checkpoint_balance DECIMAL(20, 9);
    checkpoint_seq BIGINT;
    delta_sum DECIMAL(20, 9);
BEGIN
    -- Get checkpoint for this user (may not exist for new users)
    SELECT c.balance, c.checkpoint_seq INTO checkpoint_balance, checkpoint_seq
    FROM user_balance_checkpoints c
    WHERE c.user_id = NEW.user_id;

    -- Calculate old balance (before this transaction)
    IF checkpoint_balance IS NULL THEN
        -- No checkpoint exists, sum all transactions except the new one
        SELECT COALESCE(SUM(
            CASE WHEN transaction_type IN ('admin_grant', 'purchase') THEN amount ELSE -amount END
        ), 0) INTO old_balance
        FROM credits_transactions
        WHERE user_id = NEW.user_id AND id != NEW.id;
    ELSE
        -- Sum transactions after checkpoint but before this one (using seq for reliable ordering)
        SELECT COALESCE(SUM(
            CASE WHEN transaction_type IN ('admin_grant', 'purchase') THEN amount ELSE -amount END
        ), 0) INTO delta_sum
        FROM credits_transactions
        WHERE user_id = NEW.user_id
          AND seq > checkpoint_seq
          AND id != NEW.id;

        old_balance := checkpoint_balance + delta_sum;
    END IF;

    -- Calculate new balance based on transaction type
    IF NEW.transaction_type IN ('admin_grant', 'purchase') THEN
        new_balance := old_balance + NEW.amount;
    ELSE
        new_balance := old_balance - NEW.amount;
    END IF;

    -- Check if we crossed the zero threshold in either direction
    IF (old_balance > 0 AND new_balance <= 0) OR
       (old_balance <= 0 AND new_balance > 0) THEN
        -- Balance crossed zero threshold, notify onwards to reload config
        PERFORM pg_notify('auth_config_changed',
            json_build_object(
                'user_id', NEW.user_id,
                'old_balance', old_balance,
                'new_balance', new_balance,
                'threshold_crossed', 'zero'
            )::text
        );
    END IF;

    RETURN NULL;
END;
$$ LANGUAGE plpgsql;
