DO $$
DECLARE
    deleted_index regclass := to_regclass(format('%I.idx_batches_explicitly_deleted', current_schema()));
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_index i
        JOIN pg_class c ON c.oid = i.indexrelid
        JOIN pg_am am ON am.oid = c.relam
        WHERE i.indexrelid = deleted_index
          AND i.indrelid = to_regclass(format('%I.batches', current_schema()))
          AND am.amname = 'btree'
          AND i.indisvalid AND i.indisready AND NOT i.indisunique
          AND i.indnkeyatts = 1 AND i.indnatts = 2
          AND pg_get_indexdef(i.indexrelid, 1, true) = 'id'
          AND pg_get_indexdef(i.indexrelid, 2, true) = 'archive_bucket'
          AND i.indoption::text = '0'
          AND pg_get_expr(i.indpred, i.indrelid) = '((deleted_at IS NOT NULL) AND (retention_expired_at IS NULL))'
    ) THEN
        RAISE EXCEPTION 'idx_batches_explicitly_deleted is missing, invalid, or has the wrong definition; repair the concurrent build before upgrading';
    END IF;
END
$$;

COMMENT ON INDEX idx_batches_explicitly_deleted IS
'Explicitly deleted batches for the batch half of purge_orphaned_rows (request and archive purges). Predicate must stay identical to those queries'' static WHERE so generic plans can use it; archive_bucket is included for the archive purge.';
