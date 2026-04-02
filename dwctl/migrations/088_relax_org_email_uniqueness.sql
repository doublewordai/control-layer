-- Relax email uniqueness for organization users.
--
-- Organizations use the email field as a "contact email" which doesn't need
-- to be unique — multiple orgs may share the same contact address. Individual
-- users still need email uniqueness enforced per auth_source.
--
-- Drop the composite unique index and replace it with a partial index that
-- only enforces uniqueness for individual users.

DROP INDEX idx_users_email_auth_source_external_id;

CREATE UNIQUE INDEX idx_users_email_auth_source_external_id
ON users (email, auth_source, external_user_id) NULLS NOT DISTINCT
WHERE user_type = 'individual';
