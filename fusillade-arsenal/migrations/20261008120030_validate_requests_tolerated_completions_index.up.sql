DO $$
DECLARE
    tolerated_index regclass := to_regclass(format('%I.idx_requests_tolerated_completions', current_schema()));
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_index i
        JOIN pg_class c ON c.oid = i.indexrelid
        JOIN pg_am am ON am.oid = c.relam
        WHERE i.indexrelid = tolerated_index
          AND i.indrelid = to_regclass(format('%I.requests', current_schema()))
          AND am.amname = 'btree'
          AND i.indisvalid AND i.indisready AND NOT i.indisunique
          AND i.indnkeyatts = 2 AND i.indnatts = 3
          AND pg_get_indexdef(i.indexrelid, 1, true) = 'model'
          AND pg_get_indexdef(i.indexrelid, 2, true) = 'completed_at'
          AND pg_get_indexdef(i.indexrelid, 3, true) = 'started_at'
          AND i.indoption::text = '0 0'
          AND pg_get_expr(i.indpred, i.indrelid) = '(dispatched_tolerated AND (state = ''completed''::text))'
    ) THEN
        RAISE EXCEPTION 'idx_requests_tolerated_completions is missing, invalid, or has the wrong definition; repair the concurrent build before upgrading';
    END IF;
END
$$;

COMMENT ON INDEX idx_requests_tolerated_completions IS
'Tolerated successful completions per model for the spillover-tolerations release throughput read. Predicate must stay identical to that query''s static WHERE; started_at is included so the read is index-only.';
