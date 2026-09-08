-- Content-free request parameters and the upstream's own cached-prompt count.
--
-- These are the per-request scalars workload profiling needs and cannot get from the
-- bodies once zero-data-retention blanks them: whether the caller streamed, what output
-- cap it asked for, its sampling settings, how many messages and tool definitions it
-- sent, and how much of the prompt the ENGINE says it served from its prefix cache.
--
-- `engine_cached_tokens` is `usage.prompt_tokens_details.cached_tokens` as reported by
-- the upstream (SGLang/vLLM through the dynamo frontend, OpenAI-compatible providers).
-- It is deliberately a separate column from `cache_read_input_tokens`: that one is dwctl's
-- own cache layer and drives billing, and the batcher must keep ignoring provider-native
-- counts there (see serializers::cache_tokens_from_usage). This column is observational
-- and is never priced. NULL when the upstream reported nothing.
--
-- `stream` is false when the request omitted the field, NULL only when the body did not
-- parse as a typed chat/completions request (embeddings, the Responses API, opaque bodies).
-- `max_tokens` is `max_completion_tokens` when present, else `max_tokens`.
--
-- Nullable, no defaults, no backfill: historical rows stay NULL. For the warehouse copy
-- (ClickPipes lands every column non-Nullable) NULL arrives as 0 / false / '' -- read
-- "stream = false AND message_count = 0" as "predates migration 138".
ALTER TABLE http_analytics ADD COLUMN IF NOT EXISTS engine_cached_tokens BIGINT;
ALTER TABLE http_analytics ADD COLUMN IF NOT EXISTS stream BOOLEAN;
ALTER TABLE http_analytics ADD COLUMN IF NOT EXISTS max_tokens BIGINT;
ALTER TABLE http_analytics ADD COLUMN IF NOT EXISTS temperature REAL;
ALTER TABLE http_analytics ADD COLUMN IF NOT EXISTS top_p REAL;
ALTER TABLE http_analytics ADD COLUMN IF NOT EXISTS n INTEGER;
ALTER TABLE http_analytics ADD COLUMN IF NOT EXISTS tool_count INTEGER;
ALTER TABLE http_analytics ADD COLUMN IF NOT EXISTS message_count INTEGER;

-- No indexes: written on every request, read only by aggregate scans that already filter
-- on timestamp (and, for anything heavy, by the ClickHouse copy).
