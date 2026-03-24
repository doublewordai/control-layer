-- Add trace_id column to http_analytics for correlation with outlet-postgres
-- and Grafana Tempo trace lookups.
ALTER TABLE http_analytics ADD COLUMN IF NOT EXISTS trace_id TEXT;

CREATE INDEX IF NOT EXISTS idx_analytics_trace_id ON http_analytics (trace_id) WHERE trace_id IS NOT NULL;
