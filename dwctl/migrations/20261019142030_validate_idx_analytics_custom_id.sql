DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
        FROM pg_index i
        JOIN pg_class c ON c.oid = i.indexrelid
        JOIN pg_am am ON am.oid = c.relam
        WHERE i.indexrelid = to_regclass(format('%I.idx_analytics_custom_id', current_schema()))
          AND i.indrelid = to_regclass(format('%I.http_analytics', current_schema()))
          AND am.amname = 'btree'
          AND i.indisvalid
          AND i.indisready
          AND NOT i.indisunique
          AND NOT i.indisexclusion
          AND i.indnkeyatts = 1
          AND i.indnatts = 1
          AND i.indoption::text = '0'
          AND pg_get_indexdef(i.indexrelid, 1, true) = 'custom_id'
          AND pg_get_expr(i.indpred, i.indrelid) = '(custom_id IS NOT NULL)'
    ) THEN
        RAISE EXCEPTION 'idx_analytics_custom_id is missing, invalid, not ready, or has the wrong definition';
    END IF;
END
$$;

COMMENT ON INDEX idx_analytics_custom_id IS
    'Exact case-sensitive lookup for request analytics by caller-supplied custom_id.';
