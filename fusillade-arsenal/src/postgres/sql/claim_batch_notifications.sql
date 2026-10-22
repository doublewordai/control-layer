WITH claimed AS (
    UPDATE batches b
    SET notification_sent_at = NOW()
    WHERE b.id IN (
        -- SKIP LOCKED so concurrent pollers on other replicas
        -- claim disjoint sets instead of queueing on row locks;
        -- oldest-frozen-first so nothing starves under a backlog.
        SELECT id FROM batches
        WHERE counts_frozen_at IS NOT NULL
          AND notification_sent_at IS NULL
          AND cancelling_at IS NULL
          AND deleted_at IS NULL
          AND total_requests > 0
        ORDER BY counts_frozen_at
        LIMIT 100
        FOR UPDATE SKIP LOCKED
    )
      AND b.notification_sent_at IS NULL  -- Re-check to handle concurrent pollers
    RETURNING b.id, b.file_id, b.endpoint, b.service_tier, b.completion_window, b.metadata,
              b.output_file_id, b.error_file_id, b.created_by, b.created_at,
              b.expires_at, b.cancelling_at, b.errors, b.total_requests,
              b.requests_started_at, b.finalizing_at, b.completed_at,
              b.failed_at, b.cancelled_at, b.deleted_at, b.notification_sent_at, b.api_key_id,
              b.completed_requests, b.failed_requests, b.canceled_requests,
              b.archive_bucket
)
SELECT u.id AS "id!", u.file_id, u.endpoint AS "endpoint!",
       u.service_tier AS "service_tier?",
       u.completion_window AS "completion_window?", u.metadata,
       u.output_file_id, u.error_file_id, u.created_by AS "created_by!",
       u.created_at AS "created_at!", u.expires_at AS "expires_at?", u.cancelling_at,
       u.errors, u.total_requests AS "total_requests!",
       u.requests_started_at, u.finalizing_at, u.completed_at,
       u.failed_at, u.cancelled_at, u.deleted_at,
       u.notification_sent_at, u.api_key_id,
       u.completed_requests AS "completed_requests!",
       u.failed_requests AS "failed_requests!",
       u.canceled_requests AS "canceled_requests!",
       0::BIGINT AS "pending_requests!",
       0::BIGINT AS "in_progress_requests!",
       f.name AS "input_file_name?",
       f.description as input_file_description,
       -- Frozen batches may already have archived their rows out
       -- of `requests` by claim time; fall back to the archive
       -- for the model summary.
       COALESCE(
           (SELECT string_agg(DISTINCT r.model, ', ') FROM requests r WHERE r.batch_id = u.id),
           (SELECT string_agg(DISTINCT a.model, ', ') FROM batch_requests_archive a
            WHERE a.archive_bucket = u.archive_bucket AND a.batch_id = u.id)
       ) as model
FROM claimed u
LEFT JOIN files f ON f.id = u.file_id
