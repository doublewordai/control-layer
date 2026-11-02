DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
        FROM pg_index i
        JOIN pg_class c ON c.oid = i.indexrelid
        JOIN pg_am am ON am.oid = c.relam
        WHERE i.indexrelid = to_regclass(format('%I.idx_api_keys_secret_hash', current_schema()))
          AND i.indrelid = to_regclass(format('%I.api_keys', current_schema()))
          AND am.amname = 'btree'
          AND i.indisvalid
          AND i.indisready
          AND i.indisunique
          AND NOT i.indisexclusion
          AND NOT i.indnullsnotdistinct
          AND i.indnkeyatts = 1
          AND i.indnatts = 1
          AND i.indoption::text = '0'
          AND pg_get_indexdef(i.indexrelid, 1, true) = 'secret_hash'
          AND i.indpred IS NULL
    ) THEN
        RAISE EXCEPTION 'idx_api_keys_secret_hash is missing, invalid, not ready, or has the wrong definition';
    END IF;
END
$$;

COMMENT ON INDEX idx_api_keys_secret_hash IS
    'Unique lookup of an API key by the SHA-256 hash of its secret (DW-1005). NULLs do not collide, so rows awaiting backfill are allowed.';
