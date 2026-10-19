-- Provenance of the billed cache read: 'module' = dwctl's prefix-cache classifier
-- (explicit/auto markers — the guaranteed, TTL-smoothed system), 'engine' = the
-- upstream's own reported prefix-cache hit passed through as an implicit discount on a
-- cache-tariffed model (engine-cache passthrough: best effort, no guarantees). NULL =
-- no billed cache read (creations may still be non-zero), no cache layer on the
-- response, or a row predating this column. Observational: billing truth stays in
-- cache_read_input_tokens / total_cost; compare with engine_cached_tokens for
-- achieved-vs-billed. Additive nullable column — ClickPipes lands it non-Nullable
-- downstream (NULL arrives as ''), same as migration 138; tell the pipe owner.
ALTER TABLE http_analytics ADD COLUMN IF NOT EXISTS cache_read_source TEXT;
