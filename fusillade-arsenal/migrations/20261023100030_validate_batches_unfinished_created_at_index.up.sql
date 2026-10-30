DO $$
DECLARE
    unfinished_index regclass := to_regclass(format('%I.idx_batches_unfinished_created_at', current_schema()));
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_index i
        JOIN pg_class c ON c.oid = i.indexrelid
        JOIN pg_am am ON am.oid = c.relam
        WHERE i.indexrelid = unfinished_index
          AND i.indrelid = to_regclass(format('%I.batches', current_schema()))
          AND am.amname = 'btree'
          AND i.indisvalid AND i.indisready AND NOT i.indisunique
          AND i.indnkeyatts = 1 AND i.indnatts = 1
          AND pg_get_indexdef(i.indexrelid, 1, true) = 'created_at'
          AND i.indoption::text = '0'
          AND pg_get_expr(i.indpred, i.indrelid) = '((counts_frozen_at IS NULL) AND (deleted_at IS NULL))'
    ) THEN
        RAISE EXCEPTION 'idx_batches_unfinished_created_at is missing, invalid, or has the wrong definition; repair the concurrent build before upgrading';
    END IF;
END
$$;

COMMENT ON INDEX idx_batches_unfinished_created_at IS
'Batches still running (not frozen, not deleted) by creation time. The weekly retirement guards for batch_requests_archive and request_templates_g2 read this to find any unfinished batch created in, or reading input uploaded in, the week being dropped.';
