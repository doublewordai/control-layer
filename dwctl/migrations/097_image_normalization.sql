-- Authorisation table for the dashboard image-view endpoint.
--
-- Populated by the realtime middleware and the batch ingest path each time
-- a user submits an image. A user can view the bytes for an image only if
-- they have a row in this table for the corresponding sha256 — i.e. they
-- submitted a request that referenced it. Content-addressed deduplication
-- means the same hash can be shared by many users; each is recorded
-- separately and each is independently authorised.
CREATE TABLE IF NOT EXISTS image_access (
    user_id        UUID                     NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    sha256         BYTEA                    NOT NULL,
    mime           TEXT                     NOT NULL,
    bytes_len      BIGINT                   NOT NULL,
    first_seen_at  TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT NOW(),
    last_seen_at   TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT NOW(),
    PRIMARY KEY (user_id, sha256)
);

-- Lookup index used by garbage-collection / dedup-stats queries that want
-- to know who else has referenced a given hash.
CREATE INDEX IF NOT EXISTS idx_image_access_sha256
    ON image_access (sha256);

-- Lookup index used by the dashboard "recent images for this user" view
-- when (eventually) implemented.
CREATE INDEX IF NOT EXISTS idx_image_access_user_last_seen
    ON image_access (user_id, last_seen_at DESC);
