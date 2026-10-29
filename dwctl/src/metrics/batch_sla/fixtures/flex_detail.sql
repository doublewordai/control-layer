-- Frozen legacy alert query, with synthetic model policy and a bound clock.
WITH overdue AS (
    -- (1) Still in flight, past the deadline. Hits idx_batches_active.
    --     Deliberately unbounded: a batch stuck for days must keep alerting.
    SELECT b.id,
           b.file_id,
           EXTRACT(EPOCH FROM ($1::timestamptz - b.expires_at)) / 60 AS minutes_late
    FROM batches b
    WHERE b.completion_window = '1h'
      AND b.deleted_at    IS NULL
      AND b.completed_at  IS NULL
      AND b.failed_at     IS NULL
      AND b.cancelled_at  IS NULL
      AND b.cancelling_at IS NULL
      AND b.expires_at < $1::timestamptz

    UNION ALL

    -- (2) Reached a terminal state late, within the last hour.
    SELECT b.id,
           b.file_id,
           EXTRACT(EPOCH FROM (GREATEST(b.completed_at, b.failed_at, b.cancelled_at) - b.expires_at)) / 60 AS minutes_late
    FROM batches b
    WHERE b.completion_window = '1h'
      AND b.deleted_at IS NULL
      AND b.created_at > $1::timestamptz - INTERVAL '96 hours'
      AND GREATEST(b.completed_at, b.failed_at, b.cancelled_at) > b.expires_at
      AND GREATEST(b.completed_at, b.failed_at, b.cancelled_at) > $1::timestamptz - INTERVAL '1 hour'
), enriched AS (
    SELECT o.id, o.minutes_late, COALESCE(m.model, 'unknown') AS model
    FROM overdue o
    CROSS JOIN LATERAL (
        SELECT MIN(rt.model) AS model
        FROM request_templates_all rt
        WHERE rt.file_id = o.file_id
    ) m
)
SELECT
    model,
    MAX(minutes_late)::int AS max_minutes_late
FROM enriched
GROUP BY model
