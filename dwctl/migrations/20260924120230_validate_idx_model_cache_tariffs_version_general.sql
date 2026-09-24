-- Validate prebuilt and recovered indexes before relaxing the old uniqueness guards.
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_index i
        JOIN pg_class c ON c.oid = i.indexrelid
        JOIN pg_am am ON am.oid = c.relam
        WHERE i.indexrelid = to_regclass(format('%I.idx_model_cache_tariffs_version_general', current_schema()))
          AND i.indrelid = to_regclass(format('%I.model_cache_tariffs', current_schema()))
          AND am.amname = 'btree'
          AND i.indisvalid AND i.indisready AND i.indisunique
          AND NOT i.indisexclusion AND NOT i.indnullsnotdistinct
          AND i.indnkeyatts = 2 AND i.indnatts = 2
          AND i.indoption::text = '0 0'
          AND pg_get_indexdef(i.indexrelid, 1, true) = 'deployed_model_id'
          AND pg_get_indexdef(i.indexrelid, 2, true) = 'valid_from'
          AND pg_get_expr(i.indpred, i.indrelid) = '(user_id IS NULL)'
    ) THEN
        RAISE EXCEPTION 'idx_model_cache_tariffs_version_general is missing, invalid, not ready, or has the wrong definition; repair the concurrent build before upgrading';
    END IF;
END
$$;

COMMENT ON INDEX idx_model_cache_tariffs_version_general IS 'Preserves cache-tariff ledger uniqueness independently for general, organisation and class scopes.';
