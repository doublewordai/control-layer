-- Replace email uniqueness with separate constraints for native vs federated auth
-- This allows multiple federated identities with the same email address
-- while maintaining email uniqueness for native auth users

-- Drop the existing unique constraint on email
ALTER TABLE users DROP CONSTRAINT users_email_key;

-- For native auth users: email must be unique when external_user_id is NULL
CREATE UNIQUE INDEX idx_users_email_native_auth
ON users (email) WHERE external_user_id IS NULL;

-- For federated auth users: (email, external_user_id) must be unique when external_user_id is NOT NULL
-- This allows multiple users with the same email but different external_user_ids
CREATE UNIQUE INDEX idx_users_email_external_user_id_federated
ON users (email, external_user_id) WHERE external_user_id IS NOT NULL;

-- This allows:
-- - Native auth: Each email can only have one user with external_user_id = NULL
-- - Federated auth: Multiple identities can share an email if they have different external_user_ids
--   Example: ('user@ex.com', 'github|123') and ('user@ex.com', 'google|456') are both allowed
-- - Prevents duplicates: ('user@ex.com', 'github|123') cannot appear twice
