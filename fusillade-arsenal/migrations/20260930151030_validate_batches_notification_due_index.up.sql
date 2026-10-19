DO $$
DECLARE
    due_index regclass := to_regclass(format('%I.idx_batches_notification_due', current_schema()));
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_index i
        JOIN pg_class c ON c.oid = i.indexrelid
        JOIN pg_am am ON am.oid = c.relam
        WHERE i.indexrelid = due_index
          AND i.indrelid = to_regclass(format('%I.batches', current_schema()))
          AND am.amname = 'btree'
          AND i.indisvalid AND i.indisready AND NOT i.indisunique
          AND i.indnkeyatts = 1 AND i.indnatts = 1
          AND pg_get_indexdef(i.indexrelid, 1, true) = 'counts_frozen_at'
          AND i.indoption::text = '0'
          AND pg_get_expr(i.indpred, i.indrelid) = '((counts_frozen_at IS NOT NULL) AND (notification_sent_at IS NULL) AND (cancelling_at IS NULL) AND (deleted_at IS NULL) AND (total_requests > 0))'
    ) THEN
        RAISE EXCEPTION 'idx_batches_notification_due is missing, invalid, or has the wrong definition; repair the concurrent build before upgrading';
    END IF;
END
$$;

COMMENT ON INDEX idx_batches_notification_due IS
'Pending notifications for claim_batch_notifications, oldest-frozen first. Predicate must stay identical to that query''s static WHERE so generic plans can use it; holds only frozen, unnotified batches.';
