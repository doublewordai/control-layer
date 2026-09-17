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

-- Carry every existing organization grant across, so the first post-upgrade
-- submission of an image under a second organization (which still overwrites
-- the legacy column) cannot cost the first organization its access. Reads
-- `image_access` under a share lock only; writes go to the new table.
INSERT INTO image_access_org_grants (organization_id, sha256, granted_by, first_seen_at, last_seen_at)
SELECT organization_id, sha256, user_id, first_seen_at, last_seen_at
FROM image_access
WHERE organization_id IS NOT NULL
ON CONFLICT (organization_id, sha256) DO NOTHING;

-- Rolling-deploy compatibility: instances still on the previous release keep
-- writing ONLY `image_access` (and overwrite its legacy organization column)
-- until they drain. Mirror every organization write into the grants table
-- from inside the database, so nothing an old writer records after the
-- snapshot above is lost. The new release writes the grant itself as well;
-- the two upserts are idempotent. Drop this trigger (and the legacy column)
-- in a later contract migration once no old writer remains.
CREATE OR REPLACE FUNCTION image_access_mirror_org_grant() RETURNS trigger AS $$
BEGIN
    IF NEW.organization_id IS NOT NULL THEN
        INSERT INTO image_access_org_grants (organization_id, sha256, granted_by, first_seen_at, last_seen_at)
        VALUES (NEW.organization_id, NEW.sha256, NEW.user_id, NEW.first_seen_at, NEW.last_seen_at)
        ON CONFLICT (organization_id, sha256) DO UPDATE
        SET last_seen_at = GREATEST(image_access_org_grants.last_seen_at, EXCLUDED.last_seen_at);
    END IF;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS image_access_mirror_org_grant ON image_access;
CREATE TRIGGER image_access_mirror_org_grant
    AFTER INSERT OR UPDATE OF organization_id, last_seen_at ON image_access
    FOR EACH ROW EXECUTE FUNCTION image_access_mirror_org_grant();

-- Supports the "who else references this hash" lookups alongside
-- idx_image_access_sha256.
CREATE INDEX IF NOT EXISTS idx_image_access_org_grants_sha256
    ON image_access_org_grants (sha256);
