-- Fix concurrent credit transaction race condition
-- Change created_at default from NOW() to clock_timestamp()
--
-- NOW() returns transaction start time (frozen for entire transaction)
-- clock_timestamp() returns actual wall-clock time at moment of execution
--
-- This ensures proper ordering when multiple concurrent transactions insert
-- credit records with advisory locks, as each INSERT gets a unique timestamp
-- reflecting actual insertion order rather than transaction start order.

ALTER TABLE credits_transactions
    ALTER COLUMN created_at SET DEFAULT clock_timestamp();

-- Add comment explaining the choice
COMMENT ON COLUMN credits_transactions.created_at IS 'Timestamp when transaction was created. Uses clock_timestamp() to ensure monotonic ordering even under concurrent inserts with advisory locks.';
