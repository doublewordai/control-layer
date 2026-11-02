-- DW-1011: the secret of a key issued to another user, wrapped with the
-- keystore's wrap keyring, held until the holder's one-off reveal and nulled
-- by it. This replaces reading plaintext `secret` on reveal, so the plaintext
-- column can be dropped later. NULL for every key that has no pending reveal.
--
-- Nullable with no default: a metadata-only change, and the previous release
-- neither reads nor writes it. Its writes don't touch any column the onwards
-- NOTIFY trigger watches, so wrapping or clearing it never reloads the cache.
ALTER TABLE api_keys ADD COLUMN pending_secret BYTEA;

COMMENT ON COLUMN api_keys.pending_secret IS
    'Keystore-wrapped secret for a pending one-off reveal (secret_revealed_at IS NULL), set at issue or by reconciliation. A pending key may be NULL until reconciliation wraps it; NULL for every other key.';
