#!/bin/bash
set -euo pipefail

# Only the disposable Docker Compose test database. User deletion is deliberately
# unavailable to API credentials and SSO support, so fixture cleanup is out of band.
docker compose -f docker-compose.yml -f docker-compose.test.yml exec -T postgres \
  psql -v ON_ERROR_STOP=1 -U control_layer -d control_layer <<'SQL'
BEGIN;
CREATE TEMP TABLE test_user_ids ON COMMIT DROP AS
SELECT id FROM users
WHERE email IN (
    'user@example.org', 'user2@example.org',
    'hurl-platform-manager@example.org', 'hurl-request-viewer@example.org',
    'hurl-support-customer@example.org'
) OR email LIKE 'test-%@example.org'
  OR external_user_id LIKE 'hurl-group-user-%';

-- Keep ledger foreign keys intact, but retire fixtures and release their login
-- identifiers so another run can provision fresh accounts. Revoke credentials
-- and platform roles in the same transaction, including on a failed test run.
UPDATE api_keys SET is_deleted = true
WHERE user_id IN (SELECT id FROM test_user_ids)
   OR created_by IN (SELECT id FROM test_user_ids);
DELETE FROM user_roles WHERE user_id IN (SELECT id FROM test_user_ids);
DELETE FROM user_organizations
WHERE user_id IN (SELECT id FROM test_user_ids)
   OR organization_id IN (SELECT id FROM test_user_ids);
UPDATE users
SET email = 'deleted-' || id || '@deleted.local',
    username = 'deleted-' || id,
    external_user_id = NULL, password_hash = NULL,
    display_name = NULL, avatar_url = NULL,
    is_admin = false, is_deleted = true, updated_at = NOW()
WHERE id IN (SELECT id FROM test_user_ids);
SELECT COUNT(*) AS deleted_test_users FROM test_user_ids;
COMMIT;
SQL
