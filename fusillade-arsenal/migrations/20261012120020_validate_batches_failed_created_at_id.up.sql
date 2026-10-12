DO $$
DECLARE
    failed_index regclass := to_regclass(format('%I.idx_batches_failed_created_at_id', current_schema()));
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_index i
        JOIN pg_class c ON c.oid = i.indexrelid
        JOIN pg_am am ON am.oid = c.relam
        WHERE i.indexrelid = failed_index
          AND i.indrelid = to_regclass(format('%I.batches', current_schema()))
          AND am.amname = 'btree'
          AND i.indisvalid AND i.indisready AND NOT i.indisunique
          AND i.indnkeyatts = 2 AND i.indnatts = 2
          AND pg_get_indexdef(i.indexrelid, 1, true) = 'created_at'
          AND pg_get_indexdef(i.indexrelid, 2, true) = 'id'
          AND i.indoption::text = '0 3 3'
          AND pg_get_expr(i.indpred, i.indrelid) = '((failed_at IS NOT NULL) AND (completed_at IS NULL) AND (deleted_at IS NULL))'
    ) THEN
        RAISE EXCEPTION 'idx_batches_failed_created_at_id is missing, invalid, or has the wrong definition; repair the concurrent build before upgrading';
    END IF;
END
$$;

COMMENT ON INDEX idx_batches_failed_created_at_id IS
'Serves the customer batch list filtered to a sparse terminal status: deleted_at IS NULL AND failed_at IS NOT NULL AND completed_at IS NULL ORDER BY created_at DESC, id DESC. Holds only failed batches, so a failed-batch page stops scanning all of batches newest-first. Predicate must stay implied by the status=failed predicate push_arm emits.';
