-- Drop the stored API key secret of batches (contract step 2 of 2), in its
-- own migration so its lock on batches is not held together with the
-- template locks of 20261105123212.
--
-- Batches are dispatched by api_key_id; the previous release leaves this
-- column NULL and never reads it. Metadata-only: the old bytes stay in table
-- pages until rows are rewritten, and in WAL and backups until they expire.
SET LOCAL lock_timeout = '5s';

ALTER TABLE batches DROP COLUMN api_key;
