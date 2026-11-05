-- Drop the plaintext API key column (contract step 2 of 2).
--
-- Keys authenticate by secret_hash, and the previous release neither reads
-- nor writes api_keys.secret: new rows took the column's placeholder default.
-- Dropping it removes the plaintext of every key created before hashing.
--
-- secret_hash becomes NOT NULL. The validated CHECK (20261029130020) lets SET
-- NOT NULL skip the table scan, and is redundant afterwards. DROP COLUMN also
-- drops the column's unique constraint and index.
SET LOCAL lock_timeout = '5s';

ALTER TABLE api_keys ALTER COLUMN secret_hash SET NOT NULL;
ALTER TABLE api_keys DROP CONSTRAINT api_keys_secret_hash_not_null;
ALTER TABLE api_keys DROP COLUMN secret;
