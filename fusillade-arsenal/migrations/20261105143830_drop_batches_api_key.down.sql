SET LOCAL lock_timeout = '5s';

-- The column comes back empty; the secrets themselves are gone.
ALTER TABLE batches ADD COLUMN api_key TEXT;
