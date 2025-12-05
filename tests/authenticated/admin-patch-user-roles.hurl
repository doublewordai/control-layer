# Test that admin can update user roles and permissions are reflected immediately
# With JWT changes, roles are fetched fresh from DB on each request - no re-login needed

# Get the current user (using user_jwt to auto-create the user)
GET http://localhost:3001/admin/api/v1/users/current
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Captures]
test_user_id: jsonpath "$.id" 

# Verify the user currently has default roles (StandardUser + BatchAPIUser)
GET http://localhost:3001/admin/api/v1/users/{{test_user_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.roles" contains "StandardUser"
jsonpath "$.roles" contains "BatchAPIUser"
jsonpath "$.roles" count == 2

# Test that regular users cannot access admin endpoints
GET http://localhost:3001/admin/api/v1/users
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

GET http://localhost:3001/admin/api/v1/groups
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# Patch user role to PlatformManager
PATCH http://localhost:3001/admin/api/v1/users/{{test_user_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "roles": ["PlatformManager"]
}
HTTP 200

# Test updated permissions work immediately
GET http://localhost:3001/admin/api/v1/users
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200

GET http://localhost:3001/admin/api/v1/groups
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200

POST http://localhost:3001/admin/api/v1/groups
[Cookies]
dwctl_session: {{user_jwt}}
{
  "name": "test-group-roles-{{newUuid}}",
  "description": "Test group created by role test"
}
HTTP 201
[Captures]
test_group_id: jsonpath "$.id"

# Patch to RequestViewer (StandardUser + BatchAPIUser should be auto-added)
PATCH http://localhost:3001/admin/api/v1/users/{{test_user_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "roles": ["RequestViewer"]
}
HTTP 200
[Asserts]
jsonpath "$.roles" contains "StandardUser"
jsonpath "$.roles" contains "RequestViewer"
jsonpath "$.roles" count == 2

# Patch to empty roles (should get default StandardUser + BatchAPIUser)
PATCH http://localhost:3001/admin/api/v1/users/{{test_user_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "roles": []
}
HTTP 200
[Asserts]
jsonpath "$.roles" contains "StandardUser"
jsonpath "$.roles" count == 1

# REMOVED RE-LOGIN - not needed anymore!
# Same JWT sees updated roles immediately

# Test permissions are now restricted (should not have admin access)
GET http://localhost:3001/admin/api/v1/users
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

GET http://localhost:3001/admin/api/v1/groups
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# Cleanup - delete test group
DELETE http://localhost:3001/admin/api/v1/groups/{{test_group_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# Restore user to original default roles (StandardUser + BatchAPIUser)
# This ensures subsequent tests that use user_jwt have the expected permissions
PATCH http://localhost:3001/admin/api/v1/users/{{test_user_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "roles": ["StandardUser", "BatchAPIUser"]
}
HTTP 200
[Asserts]
jsonpath "$.roles" contains "StandardUser"
jsonpath "$.roles" contains "BatchAPIUser"
jsonpath "$.roles" count == 2