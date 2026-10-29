-- DW-1005 (expand): store a SHA-256 hash of each API key secret next to the
-- plaintext so lookups can later move off `secret`. Nullable for now: the
-- previous release keeps inserting rows without a hash until it drains, so
-- the backfill and NOT NULL check come in a later release.
SET LOCAL lock_timeout = '5s';

ALTER TABLE api_keys
    ADD COLUMN secret_hash BYTEA,
    ADD COLUMN secret_prefix TEXT;

COMMENT ON COLUMN api_keys.secret_hash IS
    'SHA-256 of the UTF-8 API key secret (32 bytes). Will replace the plaintext secret for lookups.';
COMMENT ON COLUMN api_keys.secret_prefix IS
    'Leading characters of the API key secret, kept for display once the plaintext is gone.';
