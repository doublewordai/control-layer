-- Authorisation table for the dashboard image-view endpoint.
--
-- Populated by the realtime middleware and the batch ingest path each time a
-- user submits an image. Each row attributes a submission to:
--   * user_id         — the acting human (the API key's `created_by`)
--   * organization_id — the owning organization, set ONLY when the request was
--                       made under an organization API key (`created_by` differs
--                       from the key's `user_id`); NULL for personal submissions.
--
-- The image-view endpoint authorizes `user_id = <current user>` OR
-- `organization_id = <current user's membership-validated active organization>`.
-- So a personal submission is visible only to the submitter (and never to an
-- organization), while an org-key submission is additionally visible to members
-- acting in that organization. Content-addressed deduplication means the same
-- hash can be shared by many users; each grant is recorded and authorised
-- independently.
CREATE TABLE IF NOT EXISTS image_access (
    user_id         UUID                     NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    organization_id UUID                     REFERENCES users(id) ON DELETE CASCADE,
    sha256          BYTEA                    NOT NULL,
    mime            TEXT                     NOT NULL,
    bytes_len       BIGINT                   NOT NULL,
    first_seen_at   TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT NOW(),
    last_seen_at    TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT NOW(),
    PRIMARY KEY (user_id, sha256)
);

-- Lookup index used by garbage-collection / dedup-stats queries that want
-- to know who else has referenced a given hash.
CREATE INDEX IF NOT EXISTS idx_image_access_sha256
    ON image_access (sha256);

-- Supports the organization branch of the image-view authorization lookup.
CREATE INDEX IF NOT EXISTS idx_image_access_org_sha256
    ON image_access (organization_id, sha256);

-- Lookup index used by the dashboard "recent images for this user" view
-- when (eventually) implemented.
CREATE INDEX IF NOT EXISTS idx_image_access_user_last_seen
    ON image_access (user_id, last_seen_at DESC);
