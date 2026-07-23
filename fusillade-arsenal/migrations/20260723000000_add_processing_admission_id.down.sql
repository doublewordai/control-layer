-- Operational code rollbacks should normally leave this additive nullable
-- schema in place. A database downgrade is safe only after upgraded workers
-- have stopped and every in-flight admission marker has been cleared.
DO $$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM requests
        WHERE processing_admission_id IS NOT NULL
    )
       OR EXISTS (
           SELECT 1
           FROM batch_requests_archive
           WHERE processing_admission_id IS NOT NULL
       )
    THEN
        RAISE EXCEPTION
            'cannot drop processing admission columns while non-null admission markers exist';
    END IF;
END
$$;

ALTER TABLE batch_requests_archive
DROP COLUMN processing_admission_id;

ALTER TABLE requests
DROP COLUMN processing_admission_id;
