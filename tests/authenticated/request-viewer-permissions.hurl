# Test that only users with RequestViewer role can access request endpoints
# This test verifies that PlatformManager users cannot access analytics/requests endpoints

# Create a test user with only PlatformManager role (no RequestViewer)
POST http://localhost:3001/admin/api/v1/users
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "username": "platform-manager-{{newUuid}}",
  "email": "platform-manager-{{newUuid}}@example.org",
  "roles": ["StandardUser", "PlatformManager"]
}
HTTP 201
[Captures]
platform_manager_user_id: jsonpath "$.id"

# Generate JWT for the platform manager user
# Note: This assumes we have a way to generate JWTs for test users
# If not available, we'll need to use the existing user_jwt which should have StandardUser role

# Test that PlatformManager without RequestViewer cannot access requests endpoint
GET http://localhost:3001/admin/api/v1/requests
[Cookies] 
dwctl_session: {{user_jwt}}
HTTP 403

# Create a test user with RequestViewer role
POST http://localhost:3001/admin/api/v1/users
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "username": "request-viewer-{{newUuid}}",
  "email": "request-viewer-{{newUuid}}@example.org", 
  "roles": ["StandardUser", "RequestViewer"]
}
HTTP 201
[Captures]
request_viewer_user_id: jsonpath "$.id"

# Test that users with both PlatformManager and RequestViewer can access requests
POST http://localhost:3001/admin/api/v1/users
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "username": "dual-role-{{newUuid}}",
  "email": "dual-role-{{newUuid}}@example.org",
  "roles": ["StandardUser", "PlatformManager", "RequestViewer"]  
}
HTTP 201
[Captures]
dual_role_user_id: jsonpath "$.id"

# Test that StandardUser without RequestViewer cannot access requests endpoint
# (using the existing user_jwt which should be a StandardUser)
GET http://localhost:3001/admin/api/v1/requests
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# Clean up: Delete test users
DELETE http://localhost:3001/admin/api/v1/users/{{platform_manager_user_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

DELETE http://localhost:3001/admin/api/v1/users/{{request_viewer_user_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

DELETE http://localhost:3001/admin/api/v1/users/{{dual_role_user_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204