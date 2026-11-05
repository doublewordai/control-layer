-- The shadow reference cache's prefix entries (see dwctl/src/reference_cache).
--
-- One row per prompt block boundary a principal has sent to a model in the last TTL
-- window: the cumulative content hash up to that boundary, the cumulative text bytes, and
-- the writing request's tokens per byte. The final boundary of a request also carries that
-- request's exact `prompt_tokens`. Separate from `prompt_cache_entries`, which backs billed
-- explicit caching and must not be affected by this estimate.
--
-- UNLOGGED: the rows are short-lived (a sliding TTL of minutes) shadow state, so they are
-- kept out of the WAL. A crash, failover or compute restart truncates the table; the
-- estimate then reads as misses until the window refills, and nothing billed depends on it.
-- Unlogged tables are not replicated, so every query goes to the primary.
--
-- No index on expires_at: a refresh only rewrites expires_at, which keeps it a HOT update
-- (fillfactor leaves room on the page), and the sweep scans a table that holds only a few
-- minutes of entries.
CREATE UNLOGGED TABLE reference_cache_entries (
    principal_id     UUID        NOT NULL,   -- api_keys.user_id: an org, or a personal user
    virtual_model    TEXT        NOT NULL,   -- the alias the request named
    prefix_hash      BYTEA       NOT NULL,   -- cumulative content hash up to the boundary
    cumulative_bytes BIGINT      NOT NULL,   -- text bytes of the prompt up to the boundary
    tokens_per_byte  REAL,                   -- writer's prompt_tokens / prompt bytes; NULL until it completes
    exact_tokens     INTEGER,                -- writer's prompt_tokens, on its final boundary only
    visible_at       TIMESTAMPTZ NOT NULL,   -- when the writer's prefix became usable
    expires_at       TIMESTAMPTZ NOT NULL,   -- slides forward when a request reuses the prefix
    PRIMARY KEY (principal_id, virtual_model, prefix_hash)
) WITH (fillfactor = 80);
