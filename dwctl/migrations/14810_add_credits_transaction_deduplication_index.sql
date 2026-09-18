-- no-transaction
CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS idx_credits_transactions_deduplication_id
    ON credits_transactions (deduplication_id)
    WHERE deduplication_id IS NOT NULL;
