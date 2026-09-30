DO $$
DECLARE
    cancelling_index regclass := to_regclass(format('%I.idx_batches_cancelling', current_schema()));
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_index i
        JOIN pg_class c ON c.oid = i.indexrelid
        JOIN pg_am am ON am.oid = c.relam
        WHERE i.indexrelid = cancelling_index
          AND i.indrelid = to_regclass(format('%I.batches', current_schema()))
          AND am.amname = 'btree'
          AND i.indisvalid AND i.indisready AND NOT i.indisunique
          AND i.indnkeyatts = 1 AND i.indnatts = 1
          AND pg_get_indexdef(i.indexrelid, 1, true) = 'id'
          AND i.indoption::text = '0'
          AND pg_get_expr(i.indpred, i.indrelid) = '((cancelling_at IS NOT NULL) AND (deleted_at IS NULL))'
    ) THEN
        RAISE EXCEPTION 'idx_batches_cancelling is missing, invalid, or has the wrong definition; repair the concurrent build before upgrading';
    END IF;
END
$$;

COMMENT ON INDEX idx_batches_cancelling IS
'Serves the daemon cancellation poll (get_cancelled_batch_ids): id = ANY($1) AND cancelling_at IS NOT NULL AND deleted_at IS NULL. Holds only cancelling batches, so probing every in-flight batch id stays off the batches heap. Predicate must stay implied by that query.';
