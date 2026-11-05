-- Drop the plaintext API key column (contract step 2 of 2).
--
-- Keys authenticate by secret_hash, and the previous release neither reads
-- nor writes api_keys.secret: new rows took the column's placeholder default.
-- Dropping it removes the plaintext of keys created before hashing from the
-- live schema. DROP COLUMN rewrites nothing: the old bytes stay in table
-- pages until the rows are rewritten, and in WAL and backups until they
-- expire.
--
-- secret_hash becomes NOT NULL. The validated CHECK (20261029130020) lets SET
-- NOT NULL skip the table scan, and is redundant afterwards. DROP COLUMN also
-- drops the column's unique constraint and index.
SET LOCAL lock_timeout = '5s';

ALTER TABLE api_keys ALTER COLUMN secret_hash SET NOT NULL;
ALTER TABLE api_keys DROP CONSTRAINT api_keys_secret_hash_not_null;
ALTER TABLE api_keys DROP COLUMN secret;

-- Descriptions written while the plaintext column still existed.
COMMENT ON COLUMN api_keys.secret_hash IS
    'SHA-256 of the UTF-8 API key secret (32 bytes). The only stored form of a key''s secret; keys are looked up by it.';
COMMENT ON COLUMN api_keys.secret_prefix IS
    'Leading characters of the API key secret, kept for display.';
COMMENT ON INDEX idx_api_keys_secret_hash IS
    'Unique lookup of an API key by the SHA-256 hash of its secret (DW-1005).';
