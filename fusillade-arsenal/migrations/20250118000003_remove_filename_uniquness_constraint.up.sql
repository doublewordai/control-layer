-- Remove UNIQUE constraint from files.name to allow duplicate filenames across users
-- Filenames can be scoped to users in the application layer
ALTER TABLE files
DROP CONSTRAINT files_name_unique;

-- Add comment explaining that filenames are not globally unique
COMMENT ON COLUMN files.name IS 'File name (not globally unique - scoped to users in application layer). Use file.id for all references.';