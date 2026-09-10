DO $$
DECLARE
    page_index regclass := to_regclass(format('%I.idx_batches_owner_created_at_id', current_schema()));
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_index i
        JOIN pg_class c ON c.oid = i.indexrelid
        JOIN pg_am am ON am.oid = c.relam
        WHERE i.indexrelid = page_index
          AND i.indrelid = to_regclass(format('%I.batches', current_schema()))
          AND am.amname = 'btree'
          AND i.indisvalid AND i.indisready AND NOT i.indisunique
          AND i.indnkeyatts = 3 AND i.indnatts = 3
          AND pg_get_indexdef(i.indexrelid, 1, true) = 'created_by'
          AND pg_get_indexdef(i.indexrelid, 2, true) = 'created_at'
          AND pg_get_indexdef(i.indexrelid, 3, true) = 'id'
          AND i.indoption::text = '0 3 3'
          AND pg_get_expr(i.indpred, i.indrelid) = '(deleted_at IS NULL)'
    ) THEN
        RAISE EXCEPTION 'idx_batches_owner_created_at_id is missing, invalid, or has the wrong definition; repair the concurrent build before upgrading';
    END IF;
END
$$;

COMMENT ON INDEX idx_batches_owner_created_at_id IS
'COR-537: owner-scoped chronological batch pages. Query must emit direct owner and cursor predicates for generic prepared plans. Active-first listings read the active set separately.';
