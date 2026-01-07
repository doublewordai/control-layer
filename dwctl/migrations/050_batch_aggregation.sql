-- Migration: Batch Aggregation Optimization
-- Improves transactions list endpoint performance from ~9s to <1ms by:
-- 1. Denormalizing fusillade_batch_id onto credits_transactions (eliminates JOIN)
-- 2. Pre-aggregating batch totals in batch_aggregates table
-- 3. Using lazy aggregation with is_aggregated flag

-- Step 1: Add fusillade_batch_id column to credits_transactions (denormalize from http_analytics)
ALTER TABLE credits_transactions ADD COLUMN IF NOT EXISTS fusillade_batch_id UUID;

-- Step 2: Add is_aggregated flag to track which transactions have been aggregated
ALTER TABLE credits_transactions ADD COLUMN IF NOT EXISTS is_aggregated BOOLEAN DEFAULT false;

-- Step 3: Create batch_aggregates table for pre-computed batch totals
CREATE TABLE IF NOT EXISTS batch_aggregates (
    fusillade_batch_id UUID PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES users(id),
    total_amount DECIMAL(24, 15) NOT NULL DEFAULT 0,
    transaction_count INT NOT NULL DEFAULT 0,
    max_seq BIGINT NOT NULL,
    model TEXT,
    created_at TIMESTAMPTZ DEFAULT NOW(),
    updated_at TIMESTAMPTZ DEFAULT NOW()
);

-- Step 4: Create indexes for efficient queries
-- Index for looking up aggregates by user, sorted by recency
CREATE INDEX IF NOT EXISTS idx_batch_agg_user_seq ON batch_aggregates (user_id, max_seq DESC);

-- Partial index for unaggregated batched transactions (used during lazy aggregation)
CREATE INDEX IF NOT EXISTS idx_credits_tx_unaggregated
ON credits_transactions (user_id, fusillade_batch_id)
WHERE fusillade_batch_id IS NOT NULL AND is_aggregated = false;

-- Partial index for non-batched transactions (used in optimized query)
CREATE INDEX IF NOT EXISTS idx_credits_tx_non_batched
ON credits_transactions (user_id, seq DESC)
WHERE fusillade_batch_id IS NULL;

-- Step 5: Modify trigger to allow updates to aggregation columns only
-- The original trigger blocks ALL updates; we need to allow updates to:
-- - fusillade_batch_id (backfill from http_analytics)
-- - is_aggregated (marking transactions as aggregated)
CREATE OR REPLACE FUNCTION prevent_credit_transaction_modification()
RETURNS TRIGGER AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'Credit transactions are immutable and cannot be deleted';
    END IF;

    IF TG_OP = 'UPDATE' THEN
        -- Allow updates only if immutable columns are unchanged
        -- Immutable columns: id, user_id, transaction_type, amount, source_id,
        --                    balance_after, previous_transaction_id, description, created_at, seq
        IF OLD.id = NEW.id
           AND OLD.user_id = NEW.user_id
           AND OLD.transaction_type = NEW.transaction_type
           AND OLD.amount = NEW.amount
           AND OLD.source_id = NEW.source_id
           AND OLD.balance_after IS NOT DISTINCT FROM NEW.balance_after
           AND OLD.previous_transaction_id IS NOT DISTINCT FROM NEW.previous_transaction_id
           AND OLD.description IS NOT DISTINCT FROM NEW.description
           AND OLD.created_at = NEW.created_at
           AND OLD.seq = NEW.seq
        THEN
            -- Only fusillade_batch_id and is_aggregated changed, allow it
            RETURN NEW;
        END IF;

        RAISE EXCEPTION 'Credit transactions are immutable: only fusillade_batch_id and is_aggregated can be updated';
    END IF;

    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

-- Step 6: Backfill fusillade_batch_id from http_analytics
-- This denormalizes the batch_id to avoid expensive JOINs at query time
UPDATE credits_transactions ct
SET fusillade_batch_id = ha.fusillade_batch_id
FROM http_analytics ha
WHERE ct.source_id = ha.id::text
  AND ha.fusillade_batch_id IS NOT NULL
  AND ct.fusillade_batch_id IS NULL;
