-- Add column for linking http_analytics to fusillade batches
-- This enables aggregating batch request metrics

ALTER TABLE http_analytics ADD COLUMN fusillade_batch_id UUID;

-- Index for efficient joins between http_analytics and fusillade.batches
CREATE INDEX idx_analytics_fusillade_batch_id ON http_analytics (fusillade_batch_id)
WHERE fusillade_batch_id IS NOT NULL;
