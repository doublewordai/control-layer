-- Validate prebuilt and recovered indexes before relaxing the old uniqueness guards.
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_index i
        JOIN pg_class c ON c.oid = i.indexrelid
        JOIN pg_am am ON am.oid = c.relam
        WHERE i.indexrelid = to_regclass(format('%I.idx_model_tariffs_unique_active_batch_per_sla_general', current_schema()))
          AND i.indrelid = to_regclass(format('%I.model_tariffs', current_schema()))
          AND am.amname = 'btree'
          AND i.indisvalid AND i.indisready AND i.indisunique
          AND NOT i.indisexclusion AND NOT i.indnullsnotdistinct
          AND i.indnkeyatts = 3 AND i.indnatts = 3
          AND i.indoption::text = '0 0 0'
          AND pg_get_indexdef(i.indexrelid, 1, true) = 'deployed_model_id'
          AND pg_get_indexdef(i.indexrelid, 2, true) = 'api_key_purpose'
          AND pg_get_indexdef(i.indexrelid, 3, true) = 'completion_window'
          AND pg_get_expr(i.indpred, i.indrelid) = '((valid_until IS NULL) AND (user_id IS NULL) AND ((api_key_purpose)::text = ''batch''::text) AND (completion_window IS NOT NULL))'
    ) THEN
        RAISE EXCEPTION 'idx_model_tariffs_unique_active_batch_per_sla_general is missing, invalid, not ready, or has the wrong definition; repair the concurrent build before upgrading';
    END IF;
END
$$;

COMMENT ON INDEX idx_model_tariffs_unique_active_batch_per_sla_general IS 'Enforces one active tariff per general model/purpose/window.';
