-- Never silently remove a live ownership fence. Operational code rollbacks
-- should normally leave this additive schema in place; a database downgrade is
-- safe only after every live/archive token has been cleared.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM requests WHERE attempt_id IS NOT NULL)
       OR EXISTS (
           SELECT 1
           FROM batch_requests_archive
           WHERE attempt_id IS NOT NULL
       )
    THEN
        RAISE EXCEPTION
            'cannot drop request attempt ownership columns while non-null attempt tokens exist';
    END IF;
END
$$;

-- Drop the archive twin first so the tables cannot temporarily advertise
-- archive parity while only the live column has been removed.
ALTER TABLE batch_requests_archive
DROP COLUMN attempt_id;

ALTER TABLE requests
DROP COLUMN attempt_id;
