-- Distinguish one Claimed -> Processing admission invocation from another
-- invocation using the same claim attempt. Nullable keeps old binaries and
-- rows admitted before this migration compatible during a rolling rollout.
ALTER TABLE requests
ADD COLUMN processing_admission_id UUID;

-- Keep the archive schema in parity with requests. Terminal archive moves use
-- named mappings, so old and new binaries safely leave this nullable field
-- NULL while rolling.
ALTER TABLE batch_requests_archive
ADD COLUMN processing_admission_id UUID;

COMMENT ON COLUMN requests.processing_admission_id IS
    'Per-invocation identity for an in-progress Claimed-to-Processing commit; NULL outside Processing';

COMMENT ON COLUMN batch_requests_archive.processing_admission_id IS
    'Transient processing admission identity mirrored for schema parity; terminal archived rows store NULL';
