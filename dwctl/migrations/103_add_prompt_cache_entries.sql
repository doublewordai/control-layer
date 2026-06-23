-- Cached-input pricing: the prefix index (plan §8.1).
--
-- A *cache, not a ledger*: billing truth is credits_transactions (§8.4). Losing an
-- entry degrades to "cache miss / full price" (safe). Never walked to reprice.
--
-- Scope (org_id, virtual_model, tokenizer_version) keys each prefix:
--   - org_id            = target_user_id (org or personal user = api_key.user_id),
--                         so all of a customer's modalities share one cache scope.
--   - virtual_model     = the user-facing alias (deployed_models.alias / OriginalModel),
--                         NOT the rewritten underlying model_name — all routes of a
--                         virtual model share the alias and tokenizer.
--   - tokenizer_version = emitted by tokenizer-svc; re-keys entries on a tokenizer
--                         change so stale prefixes age out by TTL (§12).
CREATE TABLE prompt_cache_entries (
    id                     BIGSERIAL   PRIMARY KEY,
    org_id                 UUID        NOT NULL,
    virtual_model          TEXT        NOT NULL,
    tokenizer_version      TEXT        NOT NULL,
    prefix_hash            BYTEA       NOT NULL,   -- cumulative hash up to the breakpoint (content only, sans cache_control)
    cumulative_token_count INTEGER     NOT NULL,   -- tokens of the prefix ending here (stored at write; reused on read)
    ttl_tier               TEXT        NOT NULL CHECK (ttl_tier IN ('5m', '1h', '24h')),
    created_at             TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at             TIMESTAMPTZ NOT NULL,   -- slides forward on every read (§1 sliding window)
    -- The lookup key. Doubles as the btree backing point lookups by
    -- (org, model, tok, hash) — see the WHERE below.
    UNIQUE (org_id, virtual_model, tokenizer_version, prefix_hash)
);

-- Sweep / expiry support. (now() can't live in a partial-index predicate, so the
-- "active" filter is applied at query time, not baked into the index — §6.4.)
CREATE INDEX idx_prompt_cache_entries_expires_at ON prompt_cache_entries (expires_at);

-- Lookup shape (PostgresIndex):
--   SELECT prefix_hash, cumulative_token_count, ttl_tier, expires_at
--   FROM prompt_cache_entries
--   WHERE org_id = $1 AND virtual_model = $2 AND tokenizer_version = $3
--     AND prefix_hash = ANY($4) AND expires_at > now();
