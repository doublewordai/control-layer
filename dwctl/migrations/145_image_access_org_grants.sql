-- Organization grants for submitted images, one row per (organization, image).
--
-- `image_access` is keyed by (user_id, sha256) and holds ONE organization per
-- user and image, so a member who submitted the same image under a second
-- organization overwrote the first organization's grant and its other members
-- lost access (a 403 on the console view, and on re-submitting the image's
-- `dw-img://` token). Organization grants now live here, distinct per
-- organization; `image_access` keeps recording the submitter (and, for
-- rolling-deploy compatibility, its legacy `organization_id`, which the
-- authorisation lookup still honours for rows written before this table).
--
-- Additive only: no constraint on the live table changes, so instances still
-- running the previous release keep writing `image_access` unchanged, and
-- creating an empty table takes no lock on it.
CREATE TABLE IF NOT EXISTS image_access_org_grants (
    organization_id UUID                     NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    sha256          BYTEA                    NOT NULL,
    -- The member whose submission created the grant (audit only).
    granted_by      UUID                     REFERENCES users(id) ON DELETE SET NULL,
    first_seen_at   TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT NOW(),
    last_seen_at    TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT NOW(),
    PRIMARY KEY (organization_id, sha256)
);

-- Supports the "who else references this hash" lookups alongside
-- idx_image_access_sha256.
CREATE INDEX IF NOT EXISTS idx_image_access_org_grants_sha256
    ON image_access_org_grants (sha256);
