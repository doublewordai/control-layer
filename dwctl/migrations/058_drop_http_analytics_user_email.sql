-- Drop the denormalized user_email column from http_analytics
-- Email can now be fetched via JOIN with users table using user_id
-- This removes PII from denormalized storage - when users are deleted,
-- their email is scrubbed from the users table, but this column would have retained it.
ALTER TABLE http_analytics DROP COLUMN IF EXISTS user_email;
