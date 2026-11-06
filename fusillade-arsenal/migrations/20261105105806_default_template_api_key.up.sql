-- Stop storing API key secrets on request templates (contract step 1 of 2).
--
-- Requests are dispatched by API key id: batches.api_key_id for batch
-- requests, request_templates_g2.api_key_id for batchless ones. From this
-- release the application no longer names request_templates_g2.api_key, so
-- new templates take this default. The column stays NOT NULL because the
-- previous release's daemons still read it during this rollout; they also
-- dispatch by id first, so they never use the empty value. batches.api_key
-- is already nullable and new batches leave it NULL. The next release drops
-- both columns.
--
-- Metadata-only: SET DEFAULT recurses to every weekly partition and rewrites
-- nothing.
SET LOCAL lock_timeout = '5s';

ALTER TABLE request_templates_g2 ALTER COLUMN api_key SET DEFAULT '';
