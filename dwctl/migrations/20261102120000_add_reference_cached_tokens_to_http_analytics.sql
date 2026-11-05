-- Shadow "reference cache" estimate: how many prompt tokens an ideal per-customer prefix
-- cache would have served for this request (see dwctl/src/reference_cache). Written only
-- for chat completions from principals allowlisted in `reference_cache.orgs`; NULL for
-- everything else, for historical rows, and when the estimate failed. Observational: it is
-- never priced and does not change `cache_read_input_tokens` or `engine_cached_tokens`.
SET LOCAL lock_timeout = '5s';

ALTER TABLE http_analytics ADD COLUMN reference_cached_tokens BIGINT;

COMMENT ON COLUMN http_analytics.reference_cached_tokens IS
    'Shadow estimate of prompt tokens an ideal per-principal prefix cache would have served; NULL when not estimated';
