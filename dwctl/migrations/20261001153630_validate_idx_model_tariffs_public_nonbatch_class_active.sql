DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_index i
        JOIN pg_class c ON c.oid = i.indexrelid
        JOIN pg_am am ON am.oid = c.relam
        WHERE i.indexrelid = to_regclass(format('%I.idx_model_tariffs_public_nonbatch_class_active', current_schema()))
          AND i.indrelid = to_regclass(format('%I.model_tariffs', current_schema()))
          AND am.amname = 'btree' AND i.indisvalid AND i.indisready AND i.indisunique
          AND NOT i.indisexclusion AND i.indnullsnotdistinct
          AND i.indnkeyatts = 3 AND i.indnatts = 3
          AND i.indoption::text = '0 0 0'
          AND pg_get_indexdef(i.indexrelid, 1, true) = 'deployed_model_id'
          AND pg_get_indexdef(i.indexrelid, 2, true) = 'serving_class'
          AND pg_get_indexdef(i.indexrelid, 3, true) = 'api_key_purpose'
          AND pg_get_expr(i.indpred, i.indrelid) = '((user_id IS NULL) AND (valid_until IS NULL) AND ((api_key_purpose)::text <> ''batch''::text))'
    ) THEN
        RAISE EXCEPTION 'idx_model_tariffs_public_nonbatch_class_active is missing, invalid, not ready, or has the wrong definition';
    END IF;
END
$$;
COMMENT ON INDEX idx_model_tariffs_public_nonbatch_class_active IS 'Public non-batch tariffs remain unique per model/class/purpose regardless of completion window; complements batch-window uniqueness.';
