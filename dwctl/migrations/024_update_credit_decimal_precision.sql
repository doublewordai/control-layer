-- Update credit decimal precision from DECIMAL(12, 8) to DECIMAL(12, 2)
-- This allows for larger balances (up to 9,999,999,999.99) with 2 decimal places
-- instead of smaller balances (up to 9,999.99999999) with 8 decimal places
-- Existing values will be rounded to 2 decimal places

-- Update amount column
ALTER TABLE credits_transactions
    ALTER COLUMN amount TYPE DECIMAL(12, 2);

-- Update balance_after column
ALTER TABLE credits_transactions
    ALTER COLUMN balance_after TYPE DECIMAL(12, 2);

-- Comments
COMMENT ON COLUMN credits_transactions.amount IS 'Absolute value of transaction amount (up to 9,999,999,999.99)';
COMMENT ON COLUMN credits_transactions.balance_after IS 'Balance after this transaction (up to 9,999,999,999.99)';
