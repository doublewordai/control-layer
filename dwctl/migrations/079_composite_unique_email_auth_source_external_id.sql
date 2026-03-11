-- Replace separate email uniqueness indexes with a single composite index
-- on (email, auth_source, external_user_id). This allows organizations and
-- individual users to share the same email address, since they have different
-- auth_source values ('organization' vs 'native'/'proxy_header'/etc).

-- Drop the two partial indexes from migration 033
DROP INDEX idx_users_email_native_auth;
DROP INDEX idx_users_email_external_user_id_federated;

-- Single composite unique index covering all auth types.
-- NULLS NOT DISTINCT ensures that (email, auth_source, NULL) is treated as
-- a single value rather than allowing unlimited NULL duplicates.
CREATE UNIQUE INDEX idx_users_email_auth_source_external_id
ON users (email, auth_source, external_user_id) NULLS NOT DISTINCT;
