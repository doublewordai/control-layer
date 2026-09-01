-- URL of the upstream that actually served the request, read from the onwards
-- ServedBy response extension at capture time. For composite models this is
-- the selected component's endpoint (after any fallback), enabling
-- per-component attribution (owned vs external provider) of traffic and spend.
-- NULL for requests that never reached an upstream, and for rows predating
-- this column.
ALTER TABLE http_analytics ADD COLUMN served_by TEXT;
