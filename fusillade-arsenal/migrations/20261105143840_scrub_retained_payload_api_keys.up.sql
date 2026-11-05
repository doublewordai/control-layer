-- no-transaction
-- Remove stored API key secrets from retained response snapshots.
--
-- Releases that stored a key on every request template copied it into the
-- template snapshot of each retained request object
-- (payload -> 'template' -> 'api_key'). Readers ignore the field; this strips
-- it from every payload that still carries it. Group objects hold no
-- template, and no other retained column holds a key.
--
-- One statement in a no-transaction file, committing after each batch of
-- primary-key ranges (retained_response_objects is partitioned by delete_on,
-- with primary key (delete_on, object_kind, object_id)), so no lock is held
-- across the whole table. The keyset starts from sentinels below every real
-- key, so each batch is a plain row comparison that seeks the primary-key
-- index, and `delete_on >=` prunes the partitions already done. Committed
-- batches persist, so an interrupted run resumes where it stopped and a re-run
-- changes nothing. lock_timeout is per batch, so a batch blocked behind a
-- partition being retired fails fast and the Job retries.
--
-- The rewritten rows drop the old bytes from the live table once vacuumed;
-- WAL and backups keep them until they expire.
DO $$
DECLARE
    last_delete_on DATE := '-infinity';
    last_object_id UUID := '00000000-0000-0000-0000-000000000000';
    end_delete_on  DATE;
    end_object_id  UUID;
BEGIN
    LOOP
        PERFORM set_config('lock_timeout', '5s', true);

        -- Last key of the next batch of request objects.
        SELECT batch.delete_on, batch.object_id INTO end_delete_on, end_object_id
        FROM (
            SELECT delete_on, object_id
            FROM retained_response_objects
            WHERE delete_on >= last_delete_on
              AND (delete_on, object_kind, object_id) > (last_delete_on, 'request', last_object_id)
              AND object_kind = 'request'
            ORDER BY delete_on, object_kind, object_id
            LIMIT 1000
        ) batch
        ORDER BY batch.delete_on DESC, batch.object_id DESC
        LIMIT 1;
        EXIT WHEN NOT FOUND;

        UPDATE retained_response_objects
        SET payload = payload #- '{template,api_key}'
        WHERE delete_on >= last_delete_on
          AND delete_on <= end_delete_on
          AND (delete_on, object_kind, object_id) > (last_delete_on, 'request', last_object_id)
          AND (delete_on, object_kind, object_id) <= (end_delete_on, 'request', end_object_id)
          AND object_kind = 'request'
          AND payload -> 'template' ? 'api_key';

        last_delete_on := end_delete_on;
        last_object_id := end_object_id;
        COMMIT;
    END LOOP;
END
$$;
