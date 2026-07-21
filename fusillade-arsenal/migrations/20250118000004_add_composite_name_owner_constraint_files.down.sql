-- Remove composite unique constraint and index
DROP INDEX IF EXISTS idx_files_name_uploaded_by;
DROP INDEX IF EXISTS files_name_null_uploaded_by_unique;
DROP INDEX IF EXISTS files_name_uploaded_by_unique;