-- Fix credit decimal precision from DECIMAL(64, 32) to DECIMAL(24, 15)
--
-- DECIMAL(64, 32) exceeds rust_decimal's ~28 significant digit limit, causing
-- serialization panics with "CapacityError: insufficient capacity" when converting
-- high-precision decimals to strings for JSON output.
--
-- DECIMAL(24, 15) provides:
-- - Maximum balance: 999,999,999.999999999999999 (~$1 billion if 1 credit = $1)
-- - Minimum precision: 0.000000000000001 (10^-15 credits)
-- - 24 total significant digits (well within rust_decimal's 28-29 digit limit)
-- - Sufficient precision for micro-transactions (e.g., per-token AI costs)
--
-- Existing values will be rounded to 15 decimal places (no data loss expected
-- as current usage doesn't require more than 15 decimal places).

-- Update amount column
ALTER TABLE credits_transactions
    ALTER COLUMN amount TYPE DECIMAL(24, 15);

-- Update balance_after column
ALTER TABLE credits_transactions
    ALTER COLUMN balance_after TYPE DECIMAL(24, 15);

-- Update the balance threshold trigger function to match the new precision
CREATE OR REPLACE FUNCTION notify_on_balance_threshold_crossing() RETURNS trigger AS $$
DECLARE
    old_balance DECIMAL(24, 15);
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

-- Update comments
COMMENT ON COLUMN credits_transactions.amount IS 'Absolute value of transaction amount (max 999,999,999.999999999999999, 15 decimal places)';
COMMENT ON COLUMN credits_transactions.balance_after IS 'Balance after this transaction (max 999,999,999.999999999999999, 15 decimal places)';
