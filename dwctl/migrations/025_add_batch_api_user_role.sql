-- Add BatchAPIUser to roles for file management in Batch API
-- This role allows users to upload, view, and delete their own files

-- sqlx:no-transaction

-- Add new value to existing user_role enum
ALTER TYPE user_role ADD VALUE IF NOT EXISTS 'BATCHAPIUSER';