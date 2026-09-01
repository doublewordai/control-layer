-- Re-add UNIQUE constraint to files.name
-- NOTE: This is a best-effort rollback. If duplicate filenames exist,
-- the constraint will not be re-added. This is acceptable because:
-- 1. Rolling back this migration means reverting to a known-bad state
-- 2. The application already handles non-unique filenames correctly via user scoping
-- 3. Manual cleanup required if strict uniqueness is needed

-- Attempt to add constraint, ignore if it fails
DO $$
BEGIN
    ALTER TABLE files ADD CONSTRAINT files_name_unique UNIQUE (name);
EXCEPTION
    WHEN unique_violation THEN
        RAISE NOTICE 'Skipped adding UNIQUE constraint on files.name: duplicate filenames exist. This is expected if files were created after removing the constraint.';
    WHEN duplicate_object THEN
        RAISE NOTICE 'UNIQUE constraint files_name_unique already exists.';
END $$;