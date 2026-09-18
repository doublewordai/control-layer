DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
        FROM pg_index i
        JOIN pg_class c ON c.oid = i.indexrelid
        JOIN pg_am am ON am.oid = c.relam
        WHERE i.indexrelid = to_regclass('idx_credits_transactions_deduplication_id')
          AND i.indrelid = 'credits_transactions'::regclass
          AND am.amname = 'btree'
          AND i.indisvalid
          AND i.indisready
          AND i.indisunique
          AND i.indnkeyatts = 1
          AND i.indnatts = 1
          AND pg_get_indexdef(i.indexrelid, 1, true) = 'deduplication_id'
          AND i.indoption::text = '0'
          AND i.indexprs IS NULL
          AND pg_get_expr(i.indpred, i.indrelid) = '(deduplication_id IS NOT NULL)'
    ) THEN
        RAISE EXCEPTION 'idx_credits_transactions_deduplication_id is missing, invalid, or has the wrong definition';
    END IF;
END
$$;

COMMENT ON INDEX idx_credits_transactions_deduplication_id IS
    'Enforces one durable ledger transaction for each non-null logical idempotency key.';

-- Keep migration 147's lock + existence check for this release. The previous
-- binary uses ON CONFLICT (source_id) DO NOTHING, which does not handle a
-- conflict on this new index. Returning NULL from the trigger keeps duplicate
-- writes compatible throughout a rolling deploy; the unique index is the hard
-- invariant if any writer bypasses that cooperative path. A later release can
-- remove the ledger-side lock after every old writer has been replaced.
