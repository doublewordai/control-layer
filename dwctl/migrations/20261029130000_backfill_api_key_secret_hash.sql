-- no-transaction
-- DW-1005: hash every API key that predates the dual-write release. Must run
-- only after that release is fully rolled out, so no serving pod still inserts
-- rows without a hash. Same function as crypto::hash_api_key:
-- SHA-256 of the UTF-8 secret; secret_prefix is the first 8 characters.
--
-- One statement in a no-transaction file, committing after each batch of
-- primary-key ranges, so no lock is held across the whole table however many
-- keys there are. Committed batches persist, so a failed or interrupted run
-- resumes where it stopped. lock_timeout is per batch (transaction-local), so
-- a batch blocked behind a long-held row lock fails fast and the Job retries.
DO $$
DECLARE
    last_id   uuid;
    batch_end uuid;
BEGIN
    LOOP
        PERFORM set_config('lock_timeout', '5s', true);

        -- Last id of the next batch (max() has no uuid overload).
        SELECT id INTO batch_end
        FROM (
            SELECT id FROM api_keys
            WHERE last_id IS NULL OR id > last_id
            ORDER BY id
            LIMIT 5000
        ) batch
        ORDER BY id DESC
        LIMIT 1;
        EXIT WHEN batch_end IS NULL;

        UPDATE api_keys
        SET secret_hash = sha256(convert_to(secret, 'UTF8')),
            secret_prefix = left(secret, 8)
        WHERE (last_id IS NULL OR id > last_id)
          AND id <= batch_end
          AND secret_hash IS NULL;

        last_id := batch_end;
        COMMIT;
    END LOOP;
END
$$;
