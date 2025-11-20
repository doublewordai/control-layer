-- Add composite unique constraint on (name, uploaded_by) to enforce user scoped filename uniqueness
-- Uses NULLS NOT DISTINCT so that NULL uploaded_by values are treated as equal
-- This means: two files with name='file1' and uploaded_by=NULL would violate uniqueness
-- But: name='file1' with uploaded_by=NULL and name='file1' with uploaded_by='user1' are allowed

-- Regular unique index for non-NULL uploaded_by (allows duplicate NULLs by SQL standard)
CREATE UNIQUE INDEX files_name_uploaded_by_unique 
ON files(name, uploaded_by) 
WHERE uploaded_by IS NOT NULL;

-- Partial unique index for NULL uploaded_by (prevents duplicate system files with same name)
CREATE UNIQUE INDEX files_name_null_uploaded_by_unique 
ON files(name) 
WHERE uploaded_by IS NULL;

-- Add general index for efficient lookups
CREATE INDEX idx_files_name_uploaded_by ON files(name, uploaded_by);

-- Add comments explaining the deduplication logic
COMMENT ON INDEX files_name_uploaded_by_unique IS 
'Filenames must be unique per user (uploaded_by). Enforces uniqueness for user-uploaded files.';

COMMENT ON INDEX files_name_null_uploaded_by_unique IS 
'Filenames must be unique for system files (uploaded_by IS NULL). Prevents duplicate system files.';

COMMENT ON COLUMN files.uploaded_by IS 
'User who uploaded the file. Filenames must be unique per user, including for NULL (system files).';