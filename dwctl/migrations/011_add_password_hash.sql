-- Add password_hash column to users table for native authentication
ALTER TABLE users ADD COLUMN password_hash TEXT;

-- Create an index for faster lookups (optional but good for performance)  
CREATE INDEX IF NOT EXISTS idx_users_email_password 
ON users (email) WHERE password_hash IS NOT NULL;