-- $1: one reference time, $2: public model aliases/names, $3: excluded standard-tier models.
-- Only metadata is projected; no payload, identity, credential or free-form error columns.
WITH flex AS (
    SELECT b.id, b.file_id, EXTRACT(EPOCH FROM ($1::timestamptz - b.expires_at))::float8 AS lateness
    FROM batches b
    WHERE b.completion_window = '1h' AND b.deleted_at IS NULL
      AND b.completed_at IS NULL AND b.failed_at IS NULL AND b.cancelled_at IS NULL
      AND b.cancelling_at IS NULL AND b.expires_at < $1
    UNION ALL
    SELECT b.id, b.file_id,
           EXTRACT(EPOCH FROM (GREATEST(b.completed_at, b.failed_at, b.cancelled_at) - b.expires_at))::float8 AS lateness
    FROM batches b
    WHERE b.completion_window = '1h' AND b.deleted_at IS NULL
      AND b.created_at > $1 - INTERVAL '96 hours'
      AND GREATEST(b.completed_at, b.failed_at, b.cancelled_at) > b.expires_at
      AND GREATEST(b.completed_at, b.failed_at, b.cancelled_at) > $1 - INTERVAL '1 hour'
), flex_models AS (
    SELECT f.id, f.lateness,
           CASE WHEN m.model = ANY($2::text[]) THEN m.model
                WHEN m.model IS NULL THEN '_unknown' ELSE '_private' END AS model
    FROM flex f CROSS JOIN LATERAL (
        SELECT MIN(rt.model) AS model FROM request_templates_all rt WHERE rt.file_id = f.file_id
    ) m
), standard_outcomes AS (
    SELECT b.id, b.file_id, EXTRACT(EPOCH FROM ($1 - b.expires_at))::float8 AS lateness
    FROM batches b
    WHERE b.completion_window = '24h' AND b.deleted_at IS NULL AND b.expires_at < $1
      AND b.completed_at IS NULL AND b.failed_at IS NULL AND b.cancelled_at IS NULL AND b.cancelling_at IS NULL
    UNION ALL
    SELECT b.id, b.file_id, EXTRACT(EPOCH FROM (t.finished_at - b.expires_at))::float8 AS lateness
    FROM batches b CROSS JOIN LATERAL (VALUES (b.completed_at), (b.failed_at), (b.cancelled_at)) t(finished_at)
    WHERE b.completion_window = '24h' AND b.deleted_at IS NULL
      AND t.finished_at > b.expires_at AND t.finished_at > $1 - INTERVAL '1 hour'
), standard AS (
    SELECT b.id, MAX(b.lateness) AS lateness FROM standard_outcomes b
    -- Exclude only nonempty batches entirely assigned to excluded models.
    -- Missing metadata and mixed batches remain eligible.
    WHERE NOT EXISTS (
        SELECT 1 FROM request_templates_all rt WHERE rt.file_id = b.file_id AND rt.model = ANY($3::text[])
    ) OR EXISTS (
        SELECT 1 FROM request_templates_all rt WHERE rt.file_id = b.file_id
          AND (rt.model IS NULL OR NOT (rt.model = ANY($3::text[])))
    )
    GROUP BY b.id
), near AS (
    SELECT b.id, rt.model, EXTRACT(EPOCH FROM (b.expires_at - $1))::float8 AS remaining
    FROM batches b JOIN request_templates_all rt ON rt.file_id = b.file_id
    WHERE b.completed_at IS NULL AND b.failed_at IS NULL AND b.cancelled_at IS NULL AND b.deleted_at IS NULL
      AND b.created_at > $1 - INTERVAL '48 hours' AND b.expires_at > $1
      AND b.expires_at - $1 < GREATEST(INTERVAL '10 minutes', LEAST(INTERVAL '30 minutes', (b.expires_at - b.created_at) / 48))
      AND rt.model = ANY($2::text[])
)
SELECT 'flex'::text AS policy, '1h'::text AS completion_window, model,
       COUNT(*)::bigint AS affected_batches, MAX(lateness)::float8 AS max_lateness_seconds,
       0::float8 AS min_time_remaining_seconds
FROM flex_models GROUP BY model
UNION ALL
SELECT 'standard', '24h', '_all', COUNT(*)::bigint, COALESCE(MAX(lateness), 0)::float8, 0::float8 FROM standard
UNION ALL
SELECT 'near', '_all', model, COUNT(DISTINCT id)::bigint, 0::float8, MIN(remaining)::float8 FROM near GROUP BY model
