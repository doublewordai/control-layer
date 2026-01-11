-- Add request_origin and batch_sla columns to http_analytics
-- request_origin: "api", "frontend", or "fusillade" - derived from API key purpose
-- batch_sla: Batch completion window ("1h", "24h", etc.) - empty string for non-batch requests

ALTER TABLE http_analytics ADD COLUMN request_origin TEXT NOT NULL DEFAULT 'api';
ALTER TABLE http_analytics ADD COLUMN batch_sla TEXT NOT NULL DEFAULT '';

-- Index for efficient filtering by request origin
CREATE INDEX idx_analytics_request_origin ON http_analytics (request_origin);

-- Index for efficient filtering by batch SLA (only non-empty values)
CREATE INDEX idx_analytics_batch_sla ON http_analytics (batch_sla)
WHERE batch_sla != '';
