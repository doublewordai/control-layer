-- no-transaction

-- Async TTFT needs the originating queue request so its first-byte event can be
-- joined to the appropriate batch or batchless acceptance timestamp. Keep the
-- index partial and build it concurrently because http_analytics is write-heavy.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_analytics_async_ttft_window
ON http_analytics (
    ((timestamp AT TIME ZONE 'UTC')
        + duration_to_first_byte_ms * INTERVAL '1 millisecond') DESC,
    fusillade_request_id
)
INCLUDE (timestamp, duration_to_first_byte_ms)
WHERE request_origin = 'fusillade'
  AND batch_sla = '1h'
  AND status_code BETWEEN 200 AND 299
  AND response_type IN (
      'chat_completion_stream',
      'completion_stream',
      'response_stream'
  )
  AND duration_to_first_byte_ms IS NOT NULL
  AND fusillade_request_id IS NOT NULL;
