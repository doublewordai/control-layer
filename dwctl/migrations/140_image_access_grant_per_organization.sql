-- One image_access grant per (user, organization-or-personal, image).
--
-- The original key, (user_id, sha256), held ONE organization per user and
-- image: a member who submitted the same image under a second organization
-- overwrote the first organization's grant, and its other members lost
-- access (a 403 on the console view, and on re-submitting the image's
-- `dw-img://` token). Grants are now distinct per organization, with
-- personal submissions scoped by the nil UUID so the key is a plain
-- (non-partial) constraint that ON CONFLICT can target.
ALTER TABLE image_access DROP CONSTRAINT image_access_pkey;

ALTER TABLE image_access
    ADD COLUMN grant_scope UUID
        GENERATED ALWAYS AS (COALESCE(organization_id, '00000000-0000-0000-0000-000000000000'::uuid)) STORED;

ALTER TABLE image_access ADD PRIMARY KEY (user_id, sha256, grant_scope);
