-- Statuses that fail realtime traffic over to the next provider on top of
-- fallback_on_status. Fusillade daemon traffic runs its own retries and only
-- uses fallback_on_status.
SET LOCAL lock_timeout = '5s';

ALTER TABLE deployed_models
    ADD COLUMN fallback_realtime_on_status INTEGER[] NOT NULL DEFAULT '{}';

-- Composites that already fail over on server errors (NULL means the default
-- list, which includes them) also fail realtime traffic over when a provider
-- sheds load with 529.
UPDATE deployed_models
SET fallback_realtime_on_status = ARRAY[529]
WHERE is_composite
  AND (
      fallback_on_status IS NULL
      OR EXISTS (
          SELECT 1 FROM unnest(fallback_on_status) AS status
          WHERE status = 5 OR status BETWEEN 50 AND 59 OR status BETWEEN 500 AND 599
      )
  );
