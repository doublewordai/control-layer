-- Local/CI HTTP-test fixtures. Roles are provisioned out of band because SSO
-- support must not grant roles or mint another customer's credentials.
INSERT INTO users (username, email, external_user_id, auth_source, is_admin)
VALUES
    ('hurl-platform-manager', 'hurl-platform-manager@example.org', 'hurl-platform-manager', 'proxy-header', false),
    ('hurl-request-viewer', 'hurl-request-viewer@example.org', 'hurl-request-viewer', 'proxy-header', false),
    ('hurl-support-customer', 'hurl-support-customer@example.org', 'hurl-support-customer', 'proxy-header', false)
ON CONFLICT (username) DO NOTHING;

INSERT INTO user_roles (user_id, role)
SELECT id, 'STANDARDUSER'::user_role FROM users
WHERE external_user_id IN ('hurl-platform-manager', 'hurl-request-viewer', 'hurl-support-customer')
ON CONFLICT (user_id, role) DO NOTHING;
INSERT INTO user_roles (user_id, role)
SELECT id, 'PLATFORMMANAGER'::user_role FROM users WHERE external_user_id = 'hurl-platform-manager'
ON CONFLICT (user_id, role) DO NOTHING;
INSERT INTO user_roles (user_id, role)
SELECT id, 'REQUESTVIEWER'::user_role FROM users WHERE external_user_id = 'hurl-request-viewer'
ON CONFLICT (user_id, role) DO NOTHING;
