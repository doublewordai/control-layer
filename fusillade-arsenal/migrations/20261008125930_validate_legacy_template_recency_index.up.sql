DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_index i
        JOIN pg_class c ON c.oid = i.indexrelid
        JOIN pg_am am ON am.oid = c.relam
        WHERE i.indexrelid = to_regclass(format('%I.idx_request_templates_retirement_created_at', current_schema()))
          AND i.indrelid = to_regclass(format('%I.request_templates', current_schema()))
          AND am.amname = 'btree'
          AND i.indisvalid AND i.indisready AND i.indislive AND NOT i.indisunique
          AND i.indnkeyatts = 1 AND i.indnatts = 1
          AND pg_get_indexdef(i.indexrelid, 1, true) = 'created_at'
          AND i.indoption::text = '0'
          AND i.indpred IS NULL AND i.indexprs IS NULL
    ) THEN
        RAISE EXCEPTION USING
            ERRCODE = '55000',
            MESSAGE = 'legacy template recency index is missing, invalid, or has the wrong definition',
            HINT = 'Repair the concurrent index build before retrying heap retirement.';
    END IF;
END
$$;
