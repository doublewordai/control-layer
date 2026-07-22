-- migrations/20260116000000_remove_filename_uniqueness.up.sql
-- Remove all filename uniqueness constraints to allow duplicate filenames
-- Users can now create multiple files with the same name

-- Drop the user-scoped unique index
DROP INDEX IF EXISTS files_name_uploaded_by_unique;

-- Drop the null-scoped unique index for system files
DROP INDEX IF EXISTS files_name_null_uploaded_by_unique;

-- We keep idx_files_name_uploaded_by for query performance of files by user even without uniqueness

-- Update column comment to reflect that filenames are no longer unique
COMMENT ON COLUMN files.name IS 'File name (not unique - multiple files can have the same name). Use file.id for all references.';

COMMENT ON COLUMN files.uploaded_by IS 'User who uploaded the file. Filenames are not scoped or restricted.';