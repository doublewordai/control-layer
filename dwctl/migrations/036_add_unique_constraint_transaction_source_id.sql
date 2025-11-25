-- Add unique constraint on source_id to prevent duplicate payment processing
-- This prevents race conditions where multiple replicas process the same payment

-- First, check if there are any existing duplicates (there shouldn't be, but be safe)
DO $$
BEGIN
    IF EXISTS (
        SELECT source_id
        FROM credits_transactions
        GROUP BY source_id
        HAVING COUNT(*) > 1
    ) THEN
        RAISE EXCEPTION 'Duplicate source_id values exist in credits_transactions. Please resolve duplicates before adding constraint.';
    END IF;
END $$;

-- Add unique constraint on source_id
-- This ensures idempotency: only one transaction per payment/source can exist
ALTER TABLE credits_transactions
    ADD CONSTRAINT credits_transactions_source_id_unique UNIQUE (source_id);

COMMENT ON CONSTRAINT credits_transactions_source_id_unique ON credits_transactions
    IS 'Ensures idempotency - prevents duplicate transactions for the same payment/source';
