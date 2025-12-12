# =============================================================================
# Users Permissions Test Suite
# =============================================================================
# Tests all CRUD operations on /admin/api/v1/users endpoints
#
# Permission model:
# - StandardUser: Can only read their own user data
#   - Cannot list users (no Users::ReadAll)
#   - Can read own user with /users/{id} or /users/current (has Users::ReadOwn)
#   - Can update own profile (display_name, avatar_url) but NOT roles (has Users::UpdateOwn)
#   - Can see own credit balance with include=billing (has Credits::ReadOwn)
#   - Cannot create, update roles, or delete users
# - PlatformManager: Full access to user management
#   - Can list all users (has Users::ReadAll)
#   - Can read any user (has Users::ReadAll)
#   - Can see any user's credits with include=billing (has Credits::ReadAll)
#   - Can create, update (including roles), and delete users
# =============================================================================

# -----------------------------------------------------------------------------
# Setup: Get user IDs and create test PlatformManager
# -----------------------------------------------------------------------------

# Get user IDs
GET http://localhost:3001/admin/api/v1/users/current
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Captures]
admin_user_id: jsonpath "$.id"

GET http://localhost:3001/admin/api/v1/users/current
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Captures]
user1_id: jsonpath "$.id"

GET http://localhost:3001/admin/api/v1/users/current
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Captures]
user2_id: jsonpath "$.id"

# Create a dedicated PlatformManager for this test suite
POST http://localhost:3001/admin/api/v1/users
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "username": "test-platform-manager",
  "email": "pm@test.example.com",
  "display_name": "Test Platform Manager",
  "roles": ["PlatformManager"]
}
HTTP 201
[Captures]
pm_user_id: jsonpath "$.id"

# Create JWT for the PlatformManager user
POST http://localhost:3001/admin/api/v1/auth/admin-login
{
  "user_id": "{{pm_user_id}}"
}
HTTP 200
[Captures]
pm_jwt: cookie "dwctl_session"

# Grant some credits to users for billing tests
POST http://localhost:3001/admin/api/v1/transactions
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "user_id": "{{user1_id}}",
  "transaction_type": "admin_grant",
  "amount": "100.0",
  "source_id": "test-grant-user1",
  "description": "Credits for testing"
}
HTTP 201

POST http://localhost:3001/admin/api/v1/transactions
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "user_id": "{{pm_user_id}}",
  "transaction_type": "admin_grant",
  "amount": "200.0",
  "source_id": "test-grant-pm",
  "description": "Credits for testing"
}
HTTP 201

# =============================================================================
# User Listing (GET /users) - Requires Users::ReadAll
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser CANNOT list users
# -----------------------------------------------------------------------------

# User1 tries to list users - should be forbidden
GET http://localhost:3001/admin/api/v1/users
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User1 tries to list users with pagination - should be forbidden
GET http://localhost:3001/admin/api/v1/users?skip=0&limit=10
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User1 tries to list users with include=groups - should be forbidden
GET http://localhost:3001/admin/api/v1/users?include=groups
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User1 tries to list users with include=billing - should be forbidden
GET http://localhost:3001/admin/api/v1/users?include=billing
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User1 tries to list users with multiple includes - should be forbidden
GET http://localhost:3001/admin/api/v1/users?include=groups,billing
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# -----------------------------------------------------------------------------
# PlatformManager CAN list users (has Users::ReadAll)
# -----------------------------------------------------------------------------

# PlatformManager lists users - should succeed
GET http://localhost:3001/admin/api/v1/users
[Cookies]
dwctl_session: {{pm_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data" isCollection
jsonpath "$.total_count" exists
# Should see multiple users in the system
jsonpath "$.data[*].id" contains "{{user1_id}}"
jsonpath "$.data[*].id" contains "{{pm_user_id}}"

# PlatformManager lists users with pagination - should succeed
GET http://localhost:3001/admin/api/v1/users?skip=0&limit=5
[Cookies]
dwctl_session: {{pm_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data" isCollection

# PlatformManager lists users with include=groups - should succeed
GET http://localhost:3001/admin/api/v1/users?include=groups
[Cookies]
dwctl_session: {{pm_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data" isCollection
# Groups may or may not be present, but should not error

# PlatformManager lists users with include=billing - should succeed (has Credits::ReadAll)
GET http://localhost:3001/admin/api/v1/users?include=billing
[Cookies]
dwctl_session: {{pm_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data" isCollection
# Should see credit_balance fields for users with credits
jsonpath "$.data[?(@.id == '{{user1_id}}')].credit_balance" nth 0 exists
jsonpath "$.data[?(@.id == '{{pm_user_id}}')].credit_balance" nth 0 exists

# PlatformManager lists users with multiple includes - should succeed
GET http://localhost:3001/admin/api/v1/users?include=groups,billing
[Cookies]
dwctl_session: {{pm_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data" isCollection

# =============================================================================
# User Retrieval by ID (GET /users/{user_id})
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser can ONLY read themselves
# -----------------------------------------------------------------------------

# User1 gets their own user data - should succeed
GET http://localhost:3001/admin/api/v1/users/{{user1_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user1_id}}"
jsonpath "$.email" exists

# User1 uses /users/current - should succeed
GET http://localhost:3001/admin/api/v1/users/current
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user1_id}}"

# User1 tries to get user2's data - should be forbidden
GET http://localhost:3001/admin/api/v1/users/{{user2_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User1 tries to get admin's data - should be forbidden
GET http://localhost:3001/admin/api/v1/users/{{admin_user_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# -----------------------------------------------------------------------------
# StandardUser can see OWN credits with include=billing
# -----------------------------------------------------------------------------

# User1 gets own data with include=billing - should succeed
GET http://localhost:3001/admin/api/v1/users/{{user1_id}}?include=billing
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user1_id}}"
jsonpath "$.credit_balance" exists
jsonpath "$.credit_balance" == 100.0

# User1 uses /users/current with include=billing - should succeed
GET http://localhost:3001/admin/api/v1/users/current?include=billing
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$.credit_balance" exists

# -----------------------------------------------------------------------------
# PlatformManager can read ANY user (has Users::ReadAll)
# -----------------------------------------------------------------------------

# PlatformManager gets user1's data - should succeed
GET http://localhost:3001/admin/api/v1/users/{{user1_id}}
[Cookies]
dwctl_session: {{pm_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user1_id}}"

# PlatformManager gets their own data - should succeed
GET http://localhost:3001/admin/api/v1/users/{{pm_user_id}}
[Cookies]
dwctl_session: {{pm_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{pm_user_id}}"

# PlatformManager uses /users/current - should succeed
GET http://localhost:3001/admin/api/v1/users/current
[Cookies]
dwctl_session: {{pm_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{pm_user_id}}"

# PlatformManager gets admin's data - should succeed
GET http://localhost:3001/admin/api/v1/users/{{admin_user_id}}
[Cookies]
dwctl_session: {{pm_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{admin_user_id}}"

# -----------------------------------------------------------------------------
# PlatformManager can see ANY user's credits (has Credits::ReadAll)
# -----------------------------------------------------------------------------

# PlatformManager gets user1's data with include=billing - should succeed
GET http://localhost:3001/admin/api/v1/users/{{user1_id}}?include=billing
[Cookies]
dwctl_session: {{pm_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user1_id}}"
jsonpath "$.credit_balance" exists
jsonpath "$.credit_balance" == 100.0

# PlatformManager gets their own data with include=billing - should succeed
GET http://localhost:3001/admin/api/v1/users/{{pm_user_id}}?include=billing
[Cookies]
dwctl_session: {{pm_jwt}}
HTTP 200
[Asserts]
jsonpath "$.credit_balance" exists
jsonpath "$.credit_balance" == 200.0

# PlatformManager gets admin's data with include=billing - should succeed
GET http://localhost:3001/admin/api/v1/users/{{admin_user_id}}?include=billing
[Cookies]
dwctl_session: {{pm_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{admin_user_id}}"
jsonpath "$.credit_balance" exists

# =============================================================================
# User Creation (POST /users) - Requires Users::CreateAll
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser CANNOT create users
# -----------------------------------------------------------------------------

# User1 tries to create a user - should be forbidden
POST http://localhost:3001/admin/api/v1/users
[Cookies]
dwctl_session: {{user_jwt}}
{
  "username": "test-user-1",
  "email": "testuser1@example.com",
  "roles": ["StandardUser"]
}
HTTP 403

# User1 tries to create an admin user - should be forbidden
POST http://localhost:3001/admin/api/v1/users
[Cookies]
dwctl_session: {{user_jwt}}
{
  "username": "test-admin",
  "email": "testadmin@example.com",
  "roles": ["PlatformManager"]
}
HTTP 403

# -----------------------------------------------------------------------------
# PlatformManager CAN create users (has Users::CreateAll)
# -----------------------------------------------------------------------------

# PlatformManager creates a new standard user - should succeed
POST http://localhost:3001/admin/api/v1/users
[Cookies]
dwctl_session: {{pm_jwt}}
{
  "username": "created-user-1",
  "email": "createduser1@example.com",
  "display_name": "Created User 1",
  "roles": ["StandardUser"]
}
HTTP 201
[Asserts]
jsonpath "$.id" exists
jsonpath "$.username" == "created-user-1"
jsonpath "$.email" == "createduser1@example.com"
jsonpath "$.roles" contains "StandardUser"
[Captures]
created_user1_id: jsonpath "$.id"

# PlatformManager creates another user with multiple roles - should succeed
POST http://localhost:3001/admin/api/v1/users
[Cookies]
dwctl_session: {{pm_jwt}}
{
  "username": "created-user-2",
  "email": "createduser2@example.com",
  "roles": ["StandardUser", "BatchAPIUser"]
}
HTTP 201
[Asserts]
jsonpath "$.id" exists
jsonpath "$.roles" contains "StandardUser"
jsonpath "$.roles" contains "BatchAPIUser"
[Captures]
created_user2_id: jsonpath "$.id"

# =============================================================================
# User Update (PATCH /users/{user_id})
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser CAN update their own profile (display_name, avatar_url)
# -----------------------------------------------------------------------------

# User1 updates their own display name - should succeed
PATCH http://localhost:3001/admin/api/v1/users/{{user1_id}}
[Cookies]
dwctl_session: {{user_jwt}}
{
  "display_name": "User1 Updated Display Name"
}
HTTP 200
[Asserts]
jsonpath "$.display_name" == "User1 Updated Display Name"
jsonpath "$.roles" contains "StandardUser"

# User1 updates their avatar URL - should succeed
PATCH http://localhost:3001/admin/api/v1/users/{{user1_id}}
[Cookies]
dwctl_session: {{user_jwt}}
{
  "avatar_url": "https://example.com/new-avatar.jpg"
}
HTTP 200
[Asserts]
jsonpath "$.avatar_url" == "https://example.com/new-avatar.jpg"

# -----------------------------------------------------------------------------
# SECURITY: StandardUser CANNOT escalate their own privileges
# -----------------------------------------------------------------------------

# User1 tries to give themselves PlatformManager role - should be forbidden
PATCH http://localhost:3001/admin/api/v1/users/{{user1_id}}
[Cookies]
dwctl_session: {{user_jwt}}
{
  "roles": ["StandardUser", "PlatformManager"]
}
HTTP 403
[Asserts]
# Response should indicate insufficient permissions to modify roles
body contains "roles"

# Verify user1 still only has StandardUser role
GET http://localhost:3001/admin/api/v1/users/{{user1_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$.roles" count == 1
jsonpath "$.roles" contains "StandardUser"
jsonpath "$.roles" not contains "PlatformManager"

# User1 tries to remove their StandardUser role - should be forbidden
PATCH http://localhost:3001/admin/api/v1/users/{{user1_id}}
[Cookies]
dwctl_session: {{user_jwt}}
{
  "roles": []
}
HTTP 403

# User1 tries to add BatchAPIUser role - should be forbidden
PATCH http://localhost:3001/admin/api/v1/users/{{user1_id}}
[Cookies]
dwctl_session: {{user_jwt}}
{
  "roles": ["StandardUser", "BatchAPIUser"]
}
HTTP 403

# -----------------------------------------------------------------------------
# StandardUser CANNOT update other users
# -----------------------------------------------------------------------------

# User1 tries to update another user - should be forbidden
PATCH http://localhost:3001/admin/api/v1/users/{{user2_id}}
[Cookies]
dwctl_session: {{user_jwt}}
{
  "display_name": "Hacked User2"
}
HTTP 403

# -----------------------------------------------------------------------------
# PlatformManager CAN update any user (has Users::UpdateAll)
# -----------------------------------------------------------------------------

# PlatformManager updates created user1 - should succeed
PATCH http://localhost:3001/admin/api/v1/users/{{created_user1_id}}
[Cookies]
dwctl_session: {{pm_jwt}}
{
  "display_name": "Updated Display Name"
}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{created_user1_id}}"
jsonpath "$.display_name" == "Updated Display Name"

# PlatformManager updates user roles - should succeed
PATCH http://localhost:3001/admin/api/v1/users/{{created_user2_id}}
[Cookies]
dwctl_session: {{pm_jwt}}
{
  "roles": ["StandardUser", "RequestViewer"]
}
HTTP 200
[Asserts]
jsonpath "$.roles" contains "StandardUser"
jsonpath "$.roles" contains "RequestViewer"

# PlatformManager updates their own user - should succeed
PATCH http://localhost:3001/admin/api/v1/users/{{pm_user_id}}
[Cookies]
dwctl_session: {{pm_jwt}}
{
  "display_name": "PM Updated Self"
}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{pm_user_id}}"
jsonpath "$.display_name" == "PM Updated Self"

# =============================================================================
# User Deletion (DELETE /users/{user_id}) - Requires Users::DeleteAll
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser CANNOT delete users
# -----------------------------------------------------------------------------

# User1 tries to delete created user - should be forbidden
DELETE http://localhost:3001/admin/api/v1/users/{{created_user1_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User1 tries to delete themselves - should be forbidden
DELETE http://localhost:3001/admin/api/v1/users/{{user1_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User1 tries to delete another user - should be forbidden
DELETE http://localhost:3001/admin/api/v1/users/{{user2_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# -----------------------------------------------------------------------------
# PlatformManager CAN delete users (has Users::DeleteAll)
# -----------------------------------------------------------------------------

# PlatformManager deletes created user1 - should succeed
DELETE http://localhost:3001/admin/api/v1/users/{{created_user1_id}}
[Cookies]
dwctl_session: {{pm_jwt}}
HTTP 204

# Verify user is deleted - should get 404
GET http://localhost:3001/admin/api/v1/users/{{created_user1_id}}
[Cookies]
dwctl_session: {{pm_jwt}}
HTTP 404

# PlatformManager deletes created user2 - should succeed
DELETE http://localhost:3001/admin/api/v1/users/{{created_user2_id}}
[Cookies]
dwctl_session: {{pm_jwt}}
HTTP 204

# -----------------------------------------------------------------------------
# PlatformManager CANNOT delete themselves
# -----------------------------------------------------------------------------

# PlatformManager tries to delete themselves - should get 400 (cannot self-delete)
DELETE http://localhost:3001/admin/api/v1/users/{{pm_user_id}}
[Cookies]
dwctl_session: {{pm_jwt}}
HTTP 400

# =============================================================================
# Cleanup: Delete the test PlatformManager user
# =============================================================================

DELETE http://localhost:3001/admin/api/v1/users/{{pm_user_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# =============================================================================
# Cleanup Complete
# =============================================================================