DO $$
DECLARE
    late_index regclass := to_regclass(format('%I.idx_batches_finished_late', current_schema()));
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_index i
        JOIN pg_class c ON c.oid = i.indexrelid
        JOIN pg_am am ON am.oid = c.relam
        WHERE i.indexrelid = late_index
          AND i.indrelid = to_regclass(format('%I.batches', current_schema()))
          AND am.amname = 'btree'
          AND i.indisvalid AND i.indisready AND NOT i.indisunique
          AND i.indnkeyatts = 1 AND i.indnatts = 1
          AND pg_get_indexdef(i.indexrelid, 1, true) = 'id'
          AND i.indoption::text = '0'
          AND pg_get_expr(i.indpred, i.indrelid) = '((deleted_at IS NULL) AND ((completed_at > expires_at) OR (failed_at > expires_at) OR (cancelled_at > expires_at)))'
    ) THEN
        RAISE EXCEPTION 'idx_batches_finished_late is missing, invalid, or has the wrong definition; repair the concurrent build before upgrading';
    END IF;
END
$$;

COMMENT ON INDEX idx_batches_finished_late IS
'Batches that finished after their deadline (completed, failed or cancelled), for late-finish queries such as SLA-miss monitoring. A query''s <state>_at > expires_at must keep implying this predicate so the planner can use it.';
