-- A fresh ownership token will identify each daemon execution of a request.
-- The column is nullable with no default so legacy workers, proxy-owned
-- realtime requests, and terminal rows remain compatible during rollout.
ALTER TABLE requests
ADD COLUMN attempt_id UUID;

-- Keep the archive's named request-column shape in lockstep with `requests`.
-- On upgraded databases this column is physically appended after the existing
-- archive_bucket column, so all move SQL must continue to map columns by name.
ALTER TABLE batch_requests_archive
ADD COLUMN attempt_id UUID;

COMMENT ON COLUMN requests.attempt_id IS
    'Unique ownership token for the currently claimed daemon execution; NULL when no daemon attempt owns the request';

COMMENT ON COLUMN batch_requests_archive.attempt_id IS
    'Ownership token mirrored from requests; terminal archived rows normally store NULL';
