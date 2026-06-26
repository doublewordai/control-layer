-- Cached-input pricing: the prefix index.
--
-- A *cache, not a ledger*: billing truth lives in credit_transactions. Losing an
-- entry degrades to "cache miss / full price" (safe). Never walked to reprice.
--
-- Scope (principal_id, virtual_model, tokenizer_version) keys each prefix:
--   - principal_id      = the billing principal (api_keys.user_id): an org id for an org
--                         key, or a personal user id for a personal key. All requests under
--                         that principal share one cache scope — so members of an org that
--                         use org keys cache against each other, while personal keys are
--                         scoped to the individual.
--   - virtual_model     = the user-facing alias (deployed_models.alias / OriginalModel),
--                         NOT the rewritten underlying model_name — all routes of a
--                         virtual model share the alias and tokenizer.
--   - tokenizer_version = emitted by tokenizer-svc; re-keys entries on a tokenizer
--                         change so stale prefixes age out by TTL.
CREATE TABLE prompt_cache_entries (
    id                     BIGSERIAL   PRIMARY KEY,
    principal_id           UUID        NOT NULL,   -- = api_keys.user_id (billing principal: org OR personal user)
    virtual_model          TEXT        NOT NULL,
    tokenizer_version      TEXT        NOT NULL,
    prefix_hash            BYTEA       NOT NULL,   -- cumulative hash up to the breakpoint (content only, sans cache_control)
    cumulative_token_count INTEGER     NOT NULL,   -- tokens of the prefix ending here (stored at write; reused on read)
    ttl_tier               TEXT        NOT NULL CHECK (ttl_tier IN ('5m', '1h', '24h')),
    created_at             TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at             TIMESTAMPTZ NOT NULL,   -- slides forward on every read (sliding window)
    -- The lookup key. Doubles as the btree backing point lookups by
    -- (principal, model, tok, hash) — see the WHERE below.
    UNIQUE (principal_id, virtual_model, tokenizer_version, prefix_hash)
);

-- Sweep / expiry support. (now() can't live in a partial-index predicate, so the
-- "active" filter is applied at query time, not baked into the index.)
CREATE INDEX idx_prompt_cache_entries_expires_at ON prompt_cache_entries (expires_at);

-- Lookup shape (PostgresIndex):
--   SELECT prefix_hash, cumulative_token_count, ttl_tier, expires_at
--   FROM prompt_cache_entries
--   WHERE principal_id = $1 AND virtual_model = $2 AND tokenizer_version = $3
--     AND prefix_hash = ANY($4) AND expires_at > now();
