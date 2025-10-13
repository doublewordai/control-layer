# Get the current user (using user_jwt to auto-create the user)
GET http://localhost:3001/admin/api/v1/users/current
[Cookies]
clay_session: {{user_jwt}}
HTTP 200
[Captures]
test_user_id: jsonpath "$.id" 

# Verify the user currently has User role
GET http://localhost:3001/admin/api/v1/users/{{test_user_id}}
[Cookies]
clay_session: {{admin_jwt}}
HTTP 200
[Captures]
original_roles: jsonpath "$.roles"
[Asserts]
jsonpath "$.roles[0]" == "StandardUser"
jsonpath "$.roles" count == 1

# Test that regular users cannot access admin endpoints (list users)
GET http://localhost:3001/admin/api/v1/users
[Cookies]
clay_session: {{user_jwt}}
HTTP 403

# Test that regular users cannot access admin endpoints (list groups)
GET http://localhost:3001/admin/api/v1/groups
[Cookies]
clay_session: {{user_jwt}}
HTTP 403

# Patch user role to PlatformManager (StandardUser will be automatically preserved)
PATCH http://localhost:3001/admin/api/v1/users/{{test_user_id}}
[Cookies]
clay_session: {{admin_jwt}}
{
  "roles": ["PlatformManager"]
}
HTTP 200
[Asserts]
jsonpath "$.roles" contains "StandardUser"
jsonpath "$.roles" contains "PlatformManager"
jsonpath "$.roles" count == 2

# Re-login to get fresh JWT with updated roles
POST http://localhost:3001/authentication/login
{
  "email": "user@example.org",
  "password": "user_password"
}
HTTP 200
[Captures]
user_jwt: cookie "clay_session"

# Test that user with PlatformManager role can now access admin endpoints (list users)
GET http://localhost:3001/admin/api/v1/users
[Cookies]
clay_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$" isCollection

# Test that user with PlatformManager role can now access admin endpoints (list groups)
GET http://localhost:3001/admin/api/v1/groups
[Cookies]
clay_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$" isCollection

# Test that they can now access specific admin operations (create group)
POST http://localhost:3001/admin/api/v1/groups
[Cookies]
clay_session: {{user_jwt}}
{
  "name": "test-group-roles-{{newUuid}}",
  "description": "Test group created by role test"
}
HTTP 201
[Captures]
test_group_id: jsonpath "$.id"

# Try to patch user to have only RequestViewer (StandardUser should still be preserved)
PATCH http://localhost:3001/admin/api/v1/users/{{test_user_id}}
[Cookies]
clay_session: {{admin_jwt}}
{
  "roles": ["RequestViewer"]
}
HTTP 200
[Asserts]
jsonpath "$.roles" contains "StandardUser"
jsonpath "$.roles" contains "RequestViewer"
jsonpath "$.roles" count == 2

# Try to patch user to have empty roles (StandardUser should still be preserved)
PATCH http://localhost:3001/admin/api/v1/users/{{test_user_id}}
[Cookies]
clay_session: {{admin_jwt}}
{
  "roles": []
}
HTTP 200
[Asserts]
jsonpath "$.roles[0]" == "StandardUser"
jsonpath "$.roles" count == 1

# Re-login to get fresh JWT with updated roles (only StandardUser now)
POST http://localhost:3001/authentication/login
{
  "email": "user@example.org",
  "password": "user_password"
}
HTTP 200
[Captures]
user_jwt: cookie "clay_session"

# Test that user with only StandardUser role cannot access admin endpoints (list users)
GET http://localhost:3001/admin/api/v1/users
[Cookies]
clay_session: {{user_jwt}}
HTTP 403

# Test that user with only StandardUser role cannot access admin endpoints (list groups)
GET http://localhost:3001/admin/api/v1/groups
[Cookies]
clay_session: {{user_jwt}}
HTTP 403

# Clean up: Delete the test group (using admin privileges)
# @cookie_storage_clear
DELETE http://localhost:3001/admin/api/v1/groups/{{test_group_id}}
[Cookies]
clay_session: {{admin_jwt}}
HTTP 204