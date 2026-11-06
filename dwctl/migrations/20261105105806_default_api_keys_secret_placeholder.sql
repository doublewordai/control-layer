-- Stop writing API key plaintext (contract step 1 of 2).
--
-- From this release dwctl never names api_keys.secret: keys authenticate by
-- secret_hash, and the plaintext exists only in the create (or rotate)
-- response. The column stays NOT NULL and UNIQUE until the next release drops
-- it, because the previous release's pods still read and write it during this
-- rollout. Rows this release inserts take this default instead: unique, and
-- never a valid key (keys start with "sk-"). Existing values are left until
-- the column is dropped.
--
-- A default rather than a value written by the application, so this release
-- keeps working while the next release's migration drops the column.
SET LOCAL lock_timeout = '5s';

ALTER TABLE api_keys ALTER COLUMN secret SET DEFAULT ('unset-' || gen_random_uuid()::text);

COMMENT ON COLUMN api_keys.secret IS
    'Deprecated: no longer written or read. Holds the plaintext of keys created before hashing, or a placeholder. Dropped in the next release.';
