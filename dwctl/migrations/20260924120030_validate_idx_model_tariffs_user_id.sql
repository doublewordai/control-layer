-- Validate prebuilt and recovered indexes before relaxing the old uniqueness guards.
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_index i
        JOIN pg_class c ON c.oid = i.indexrelid
        JOIN pg_am am ON am.oid = c.relam
        WHERE i.indexrelid = to_regclass(format('%I.idx_model_tariffs_user_id', current_schema()))
          AND i.indrelid = to_regclass(format('%I.model_tariffs', current_schema()))
          AND am.amname = 'btree'
          AND i.indisvalid AND i.indisready AND NOT i.indisunique
          AND NOT i.indisexclusion AND NOT i.indnullsnotdistinct
          AND i.indnkeyatts = 1 AND i.indnatts = 1
          AND i.indoption::text = '0'
          AND pg_get_indexdef(i.indexrelid, 1, true) = 'user_id'
          AND pg_get_expr(i.indpred, i.indrelid) = '(user_id IS NOT NULL)'
    ) THEN
        RAISE EXCEPTION 'idx_model_tariffs_user_id is missing, invalid, not ready, or has the wrong definition; repair the concurrent build before upgrading';
    END IF;
END
$$;

COMMENT ON INDEX idx_model_tariffs_user_id IS 'Supports organisation-scoped tariff lookup and account foreign-key checks.';
