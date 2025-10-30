-- Add credit system for billing and usage tracking

-- Create transaction type enum
CREATE TYPE credit_transaction_type AS ENUM (
    'purchase',          -- User purchased credits (positive)
    'admin_grant',       -- Admin granted credits (positive)
    'admin_removal',     -- Admin removal of credits (negative)
    'usage'              -- Credits used for API request (negative)
);

-- Credit transactions table - simple ledger of all credit movements
CREATE TABLE credit_transactions (
    id BIGSERIAL PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,

    -- Transaction details
    transaction_type credit_transaction_type NOT NULL,
    amount DECIMAL(12, 8) NOT NULL,  -- Absolute value of transaction
    balance_after DECIMAL(12, 8) NOT NULL CHECK (balance_after >= 0),  -- Running balance after this transaction, cannot be negative

    -- Simple description
    description TEXT,

    -- Timestamps
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Indexes
CREATE INDEX idx_credit_transactions_user_id ON credit_transactions (user_id, created_at DESC);
CREATE INDEX idx_credit_transactions_created_at ON credit_transactions (created_at DESC);
CREATE INDEX idx_credit_transactions_type ON credit_transactions (transaction_type);

-- Comments
COMMENT ON TABLE credit_transactions IS 'Simple ledger of all credit transactions';
COMMENT ON COLUMN credit_transactions.transaction_type IS 'Type of transaction - purchase, usage or admin adjustment';
COMMENT ON COLUMN credit_transactions.amount IS 'Absolute value of transaction amount';
COMMENT ON COLUMN credit_transactions.balance_after IS 'Balance after this transaction';

-- Make the table append-only (prevent updates and deletes)
CREATE OR REPLACE FUNCTION prevent_credit_transaction_modification()
RETURNS TRIGGER AS $$
BEGIN
    RAISE EXCEPTION 'Credit transactions are immutable and cannot be modified or deleted';
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER prevent_update_credit_transactions
    BEFORE UPDATE ON credit_transactions
    FOR EACH ROW
    EXECUTE FUNCTION prevent_credit_transaction_modification();

CREATE TRIGGER prevent_delete_credit_transactions
    BEFORE DELETE ON credit_transactions
    FOR EACH ROW
    EXECUTE FUNCTION prevent_credit_transaction_modification();