-- The server-side tool loop that wrote response_steps was removed from the
-- edge in August 2026; nothing has written a row since, and the table only
-- ever linked a handful of batches. Drop it so the retained-response graph is
-- exactly a request plus its template and no retention phase has to guard
-- against step linkage. A future tool loop should rebuild as
-- "continuation as fork" against the retained store's per-object routes.
-- Dropping the FK from response_steps.request_id needs ACCESS EXCLUSIVE on
-- requests; bound the wait so a busy claim path is never queued behind us.
SET LOCAL lock_timeout = '5s';
DROP TABLE IF EXISTS response_steps;
