ALTER TABLE deployed_models
    ALTER COLUMN fallback_on_status
    SET DEFAULT '{429,499,500,502,503,504}';

UPDATE deployed_models
SET fallback_on_status = CASE
    -- NULL previously inherited the historical runtime default, so preserve
    -- that behavior while adding 499 instead of replacing it with only 499.
    WHEN fallback_on_status IS NULL THEN ARRAY[429, 499, 500, 502, 503, 504]::INTEGER[]
    ELSE array_append(fallback_on_status, 499)
END
WHERE is_composite = TRUE
  AND (
      fallback_on_status IS NULL
      OR array_position(fallback_on_status, 499) IS NULL
  );
