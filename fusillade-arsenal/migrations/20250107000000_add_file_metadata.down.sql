-- Rollback: Remove metadata columns from files table

DROP INDEX IF EXISTS idx_files_uploaded_by;
DROP INDEX IF EXISTS idx_files_expires_at;
DROP INDEX IF EXISTS idx_files_status;

ALTER TABLE files
    DROP COLUMN IF EXISTS uploaded_by,
    DROP COLUMN IF EXISTS deleted_at,
    DROP COLUMN IF EXISTS expires_at,
    DROP COLUMN IF EXISTS purpose,
    DROP COLUMN IF EXISTS error_message,
    DROP COLUMN IF EXISTS status,
    DROP COLUMN IF EXISTS size_bytes;
