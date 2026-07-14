-- no-transaction

-- Support exact time-window TTFT aggregates without scanning all analytics rows.
-- This index is partial so it only covers successful realtime streaming responses,
-- and is built concurrently because http_analytics is write-heavy.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_analytics_realtime_ttft_window
ON http_analytics (
    ((timestamp AT TIME ZONE 'UTC')
        + duration_to_first_byte_ms * INTERVAL '1 millisecond') DESC
)
INCLUDE (timestamp, duration_to_first_byte_ms)
WHERE request_origin IN ('api', 'frontend')
  AND batch_sla = ''
  AND status_code BETWEEN 200 AND 299
  AND response_type IN (
      'chat_completion_stream',
      'completion_stream',
      'response_stream'
  )
  AND duration_to_first_byte_ms IS NOT NULL;
