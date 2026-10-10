DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_index i
        JOIN pg_class c ON c.oid = i.indexrelid
        JOIN pg_am am ON am.oid = c.relam
        WHERE i.indexrelid = to_regclass(format('%I.idx_batch_capacity_reservations_released', current_schema()))
          AND i.indrelid = to_regclass(format('%I.batch_capacity_reservations', current_schema()))
          AND am.amname = 'btree' AND i.indisvalid AND i.indisready AND NOT i.indisunique
          AND i.indnkeyatts = 2 AND i.indnatts = 4
          AND i.indoption::text = '0 0'
          AND pg_get_indexdef(i.indexrelid, 1, true) = 'model_id'
          AND pg_get_indexdef(i.indexrelid, 2, true) = 'released_at'
          AND pg_get_indexdef(i.indexrelid, 3, true) = 'completion_window'
          AND pg_get_indexdef(i.indexrelid, 4, true) = 'reserved_requests'
          AND pg_get_expr(i.indpred, i.indrelid) = '(released_at IS NOT NULL)'
    ) THEN
        RAISE EXCEPTION 'idx_batch_capacity_reservations_released is missing, invalid, not ready, or has the wrong definition';
    END IF;
END
$$;
COMMENT ON INDEX idx_batch_capacity_reservations_released IS 'Batch admission: sums reservations released since an outstanding-work snapshot without scanning the whole reservation ledger.';
