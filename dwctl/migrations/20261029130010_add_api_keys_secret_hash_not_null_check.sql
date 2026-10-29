-- DW-1005: every API key must carry a hash, so a write path that forgot it
-- fails loudly instead of creating a key that can never authenticate. NOT
-- VALID skips the scan of existing rows, so the ACCESS EXCLUSIVE lock is only
-- held for the catalog change; new and updated rows are checked from now on.
SET LOCAL lock_timeout = '5s';

ALTER TABLE api_keys
    ADD CONSTRAINT api_keys_secret_hash_not_null CHECK (secret_hash IS NOT NULL) NOT VALID;
