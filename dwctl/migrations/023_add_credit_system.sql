-- Add credit system for billing and usage tracking

-- Credit transactions table - simple ledger of all credit movements
CREATE TABLE credits_transactions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id UUID NOT NULL REFERENCES users(id),

    -- Transaction details
    transaction_type TEXT NOT NULL CHECK (transaction_type IN ('purchase', 'admin_grant', 'admin_removal', 'usage')),
    amount DECIMAL(12, 8) NOT NULL,  -- Absolute value of transaction

    -- Running balance after this transaction
    balance_after DECIMAL(12, 8) NOT NULL CHECK (balance_after >= 0),  -- Running balance after this transaction, cannot be negative
    previous_transaction_id UUID REFERENCES credits_transactions(id), -- Link to previous transaction for this user

    -- Simple description
    description TEXT,

    -- Timestamps
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Indexes
CREATE INDEX idx_credits_transactions_user_id ON credits_transactions (user_id, created_at DESC);
CREATE INDEX idx_credits_transactions_created_at ON credits_transactions (created_at DESC);
CREATE INDEX idx_credits_transactions_type ON credits_transactions (transaction_type);

-- Comments
COMMENT ON TABLE credits_transactions IS 'Simple ledger of all credit transactions';
COMMENT ON COLUMN credits_transactions.transaction_type IS 'Type of transaction - purchase, usage or admin adjustment';
COMMENT ON COLUMN credits_transactions.amount IS 'Absolute value of transaction amount';
COMMENT ON COLUMN credits_transactions.balance_after IS 'Balance after this transaction';

-- Make the table append-only (prevent updates and deletes)
CREATE OR REPLACE FUNCTION prevent_credit_transaction_modification()
RETURNS TRIGGER AS $$
BEGIN
    RAISE EXCEPTION 'Credit transactions are immutable and cannot be modified or deleted';
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER prevent_update_credits_transactions
    BEFORE UPDATE ON credits_transactions
    FOR EACH ROW
    EXECUTE FUNCTION prevent_credit_transaction_modification();

CREATE TRIGGER prevent_delete_credits_transactions
    BEFORE DELETE ON credits_transactions
    FOR EACH ROW
    EXECUTE FUNCTION prevent_credit_transaction_modification();