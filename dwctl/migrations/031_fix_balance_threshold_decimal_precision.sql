-- Update credit decimal precision from DECIMAL(12, 2) to DECIMAL(64, 32)
-- This allows for high precision tracking of micro-transactions (e.g., per-token costs)
-- while still supporting large balances. Storage and performance scale with actual
-- precision of values stored, not the declared maximum, hence minimal impact on storage and performance.

-- Update amount column
ALTER TABLE credits_transactions
    ALTER COLUMN amount TYPE DECIMAL(64, 32);

-- Update balance_after column
ALTER TABLE credits_transactions
    ALTER COLUMN balance_after TYPE DECIMAL(64, 32);

-- Comments
COMMENT ON COLUMN credits_transactions.amount IS 'Absolute value of transaction amount with high precision for micro-transactions';
COMMENT ON COLUMN credits_transactions.balance_after IS 'Balance after this transaction with high precision for micro-transactions';

-- Update the balance threshold trigger function to match the new precision
CREATE OR REPLACE FUNCTION notify_on_balance_threshold_crossing() RETURNS trigger AS $$
DECLARE
    old_balance DECIMAL(64, 32);
BEGIN
    -- Get the previous balance for this user
    -- For the first transaction, old_balance will be NULL (treated as 0)
    SELECT balance_after INTO old_balance
    FROM credits_transactions
    WHERE id = NEW.previous_transaction_id;

    -- If old_balance is NULL, this is the first transaction, treat as 0
    old_balance := COALESCE(old_balance, 0);

    -- Check if we crossed the zero threshold in either direction
    IF (old_balance > 0 AND NEW.balance_after <= 0) OR
       (old_balance <= 0 AND NEW.balance_after > 0) THEN
        -- Balance crossed zero threshold, notify onwards to reload config
        PERFORM pg_notify('auth_config_changed',
            json_build_object(
                'user_id', NEW.user_id,
                'old_balance', old_balance,
                'new_balance', NEW.balance_after,
                'threshold_crossed', 'zero'
            )::text
        );
    END IF;

    RETURN NULL;
END;
$$ LANGUAGE plpgsql;
