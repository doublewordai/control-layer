-- Separate admin access into boolean flag and functional roles
-- This migration splits the current role system into:
-- 1. is_admin boolean flag for administrative privileges  
-- 2. Functional roles: PlatformManager, RequestViewer, StandardUser

-- Step 1: Add is_admin column to users table
ALTER TABLE users ADD COLUMN is_admin BOOLEAN NOT NULL DEFAULT FALSE;

-- Step 2: Set is_admin=true for users who currently have ADMIN role
UPDATE users 
SET is_admin = TRUE 
WHERE id IN (
    SELECT user_id FROM user_roles WHERE role = 'ADMIN'
);

-- Step 3: Create new role enum
CREATE TYPE new_user_role AS ENUM ('PLATFORMMANAGER', 'REQUESTVIEWER', 'STANDARDUSER');

-- Step 4: Update existing user_roles table to use new enum
-- First, create a temporary column with the new enum type
ALTER TABLE user_roles ADD COLUMN new_role new_user_role;

-- Step 5: Migrate existing roles to new functional roles
-- ADMIN -> PLATFORMMANAGER (they get admin flag set above)
-- DEVELOPER -> PLATFORMMANAGER (platform management role) 
-- USER -> STANDARDUSER (basic user functionality)
-- OBSERVER -> REQUESTVIEWER (request log viewing role)

UPDATE user_roles
SET new_role = CASE 
    WHEN role = 'ADMIN' THEN 'PLATFORMMANAGER'::new_user_role
    WHEN role = 'DEVELOPER' THEN 'PLATFORMMANAGER'::new_user_role
    WHEN role = 'OBSERVER' THEN 'REQUESTVIEWER'::new_user_role
    WHEN role = 'USER' THEN 'STANDARDUSER'::new_user_role
END;

-- Step 6: Drop old role column and enum, rename new ones
ALTER TABLE user_roles DROP COLUMN role;
DROP TYPE user_role;
ALTER TYPE new_user_role RENAME TO user_role;
ALTER TABLE user_roles RENAME COLUMN new_role TO role;
ALTER TABLE user_roles ALTER COLUMN role SET NOT NULL;

-- Step 7: Recreate the unique constraint that was lost when dropping the old role column
ALTER TABLE user_roles ADD CONSTRAINT user_roles_user_id_role_key UNIQUE (user_id, role);

-- Step 8: Create index on the new columns
CREATE INDEX idx_users_is_admin ON users(is_admin);

-- Update system user to use new structure (keep admin flag)
-- System user roles are handled by the user_roles table, so nothing to do here for roles