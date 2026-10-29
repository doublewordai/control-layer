-- Frozen legacy alert query, with synthetic model policy and a bound clock.
WITH missed_batches AS (
  SELECT
    b.id,
    b.file_id,
    b.completion_window,
    GREATEST(0, EXTRACT(EPOCH FROM ($1::timestamptz - b.expires_at)) / 60)::int AS minutes_late
  FROM batches b
  WHERE b.completion_window = '24h'
    AND b.expires_at IS NOT NULL
    AND b.deleted_at IS NULL
    AND b.expires_at < $1::timestamptz
    AND b.completed_at IS NULL
    AND b.failed_at IS NULL
    AND b.cancelled_at IS NULL
    AND b.cancelling_at IS NULL

  UNION ALL

  SELECT
    b.id,
    b.file_id,
    b.completion_window,
    GREATEST(0, EXTRACT(EPOCH FROM (b.completed_at - b.expires_at)) / 60)::int AS minutes_late
  FROM batches b
  WHERE b.completion_window = '24h'
    AND b.expires_at IS NOT NULL
    AND b.deleted_at IS NULL
    AND b.completed_at > b.expires_at
    AND b.completed_at > $1::timestamptz - INTERVAL '1 hour'

  UNION ALL

  SELECT
    b.id,
    b.file_id,
    b.completion_window,
    GREATEST(0, EXTRACT(EPOCH FROM (b.failed_at - b.expires_at)) / 60)::int AS minutes_late
  FROM batches b
  WHERE b.completion_window = '24h'
    AND b.expires_at IS NOT NULL
    AND b.deleted_at IS NULL
    AND b.failed_at > b.expires_at
    AND b.failed_at > $1::timestamptz - INTERVAL '1 hour'

  UNION ALL

  SELECT
    b.id,
    b.file_id,
    b.completion_window,
    GREATEST(0, EXTRACT(EPOCH FROM (b.cancelled_at - b.expires_at)) / 60)::int AS minutes_late
  FROM batches b
  WHERE b.completion_window = '24h'
    AND b.expires_at IS NOT NULL
    AND b.deleted_at IS NULL
    AND b.cancelled_at > b.expires_at
    AND b.cancelled_at > $1::timestamptz - INTERVAL '1 hour'
), deduped AS (
  SELECT
    id,
    completion_window,
    MAX(minutes_late) AS minutes_late
  FROM missed_batches mb
  -- Exclude only batches wholly assigned to this exact model alias.
  -- Mixed-model batches and missing template metadata remain alertable.
  WHERE NOT EXISTS (
    SELECT 1 FROM request_templates_all rt
    WHERE rt.file_id = mb.file_id
      AND rt.model = 'excluded-model'
  ) OR EXISTS (
    SELECT 1 FROM request_templates_all rt
    WHERE rt.file_id = mb.file_id
      AND rt.model IS DISTINCT FROM 'excluded-model'
  )
  GROUP BY id, completion_window
)
SELECT
  completion_window,
  COUNT(*) AS batches_missed_sla
FROM deduped
GROUP BY completion_window
