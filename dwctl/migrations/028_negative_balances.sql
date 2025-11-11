-- Alter credits transactions to allow negative balances
ALTER TABLE credits_transactions
    DROP CONSTRAINT IF EXISTS credits_transactions_balance_after_check;
-- Add check to amount field to prevent zero or less then transactions
ALTER TABLE credits_transactions
    ADD CONSTRAINT credits_transactions_amount_nonzero_check CHECK (amount > 0);