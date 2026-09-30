DO $$
DECLARE
    due_index regclass := to_regclass(format('%I.idx_files_content_expiry_due', current_schema()));
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_index i
        JOIN pg_class c ON c.oid = i.indexrelid
        JOIN pg_am am ON am.oid = c.relam
        WHERE i.indexrelid = due_index
          AND i.indrelid = to_regclass(format('%I.files', current_schema()))
          AND am.amname = 'btree'
          AND i.indisvalid AND i.indisready AND NOT i.indisunique
          AND i.indnkeyatts = 2 AND i.indnatts = 2
          AND pg_get_indexdef(i.indexrelid, 1, true) = 'created_at'
          AND pg_get_indexdef(i.indexrelid, 2, true) = 'id'
          AND i.indoption::text = '0 0'
          AND pg_get_expr(i.indpred, i.indrelid) = '((purpose = ''batch''::text) AND (deleted_at IS NULL))'
    ) THEN
        RAISE EXCEPTION 'idx_files_content_expiry_due is missing, invalid, or has the wrong definition; repair the concurrent build before upgrading';
    END IF;
END
$$;

COMMENT ON INDEX idx_files_content_expiry_due IS
'Unexpired batch input files by upload age for expire_file_content. Predicate must stay identical to that query''s static WHERE, and the age bound must stay on the bare created_at column so the scan stops at the retention horizon.';
