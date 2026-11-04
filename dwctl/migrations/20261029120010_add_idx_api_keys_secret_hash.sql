-- no-transaction
CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS idx_api_keys_secret_hash
    ON api_keys (secret_hash);
