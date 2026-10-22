DO $$
DECLARE
    deleted_index regclass := to_regclass(format('%I.idx_files_explicitly_deleted', current_schema()));
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_index i
        JOIN pg_class c ON c.oid = i.indexrelid
        JOIN pg_am am ON am.oid = c.relam
        WHERE i.indexrelid = deleted_index
          AND i.indrelid = to_regclass(format('%I.files', current_schema()))
          AND am.amname = 'btree'
          AND i.indisvalid AND i.indisready AND NOT i.indisunique
          AND i.indnkeyatts = 1 AND i.indnatts = 1
          AND pg_get_indexdef(i.indexrelid, 1, true) = 'id'
          AND i.indoption::text = '0'
          AND pg_get_expr(i.indpred, i.indrelid) = '((deleted_at IS NOT NULL) AND (retention_expired_at IS NULL))'
    ) THEN
        RAISE EXCEPTION 'idx_files_explicitly_deleted is missing, invalid, or has the wrong definition; repair the concurrent build before upgrading';
    END IF;
END
$$;

COMMENT ON INDEX idx_files_explicitly_deleted IS
'Explicitly deleted files for the template half of purge_orphaned_rows (legacy and generation-2 template purges). Predicate must stay identical to those queries'' static WHERE so generic plans can use it.';
