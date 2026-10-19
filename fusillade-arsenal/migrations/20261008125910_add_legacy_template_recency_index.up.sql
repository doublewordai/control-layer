-- no-transaction
-- Build the retirement guard's recency index before acquiring its exclusive
-- heap lock. Keep this file to one statement for concurrent index creation.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_request_templates_retirement_created_at
ON request_templates (created_at);
