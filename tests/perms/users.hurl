# =============================================================================
# Users Permissions Test Suite
# =============================================================================
# Tests all CRUD operations on /admin/api/v1/users endpoints
#
# Permission model:
# - StandardUser: Can only read their own user data
#   - Cannot list users (no Users::ReadAll)
#   - Can read own user with /users/{id} or /users/current (has Users::ReadOwn)
#   - Can see own credit balance with include=billing (has Credits::ReadOwn)
#   - Cannot create, update, or delete users
# - PlatformManager: Full access to user management
#   - Can list all users (has Users::ReadAll)
#   - Can read any user (has Users::ReadAll)
#   - Can see any user's credits with include=billing (has Credits::ReadAll)
#   - Can create, update, and delete users
# =============================================================================

# -----------------------------------------------------------------------------
# Setup: Get user IDs and grant roles
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

# Temporarily grant user2 PlatformManager role for testing
PATCH http://localhost:3001/admin/api/v1/users/{{user2_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "roles": ["PlatformManager"]
}
HTTP 200

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
  "user_id": "{{user2_id}}",
  "transaction_type": "admin_grant",
  "amount": "200.0",
  "source_id": "test-grant-user2",
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

# User2 (PlatformManager) lists users - should succeed
GET http://localhost:3001/admin/api/v1/users
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data" isCollection
jsonpath "$.total_count" exists
# Should see multiple users in the system
jsonpath "$.data[*].id" contains "{{user1_id}}"
jsonpath "$.data[*].id" contains "{{user2_id}}"

# User2 lists users with pagination - should succeed
GET http://localhost:3001/admin/api/v1/users?skip=0&limit=5
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data" isCollection

# User2 lists users with include=groups - should succeed
GET http://localhost:3001/admin/api/v1/users?include=groups
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data" isCollection
# Groups may or may not be present, but should not error

# User2 lists users with include=billing - should succeed (has Credits::ReadAll)
GET http://localhost:3001/admin/api/v1/users?include=billing
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data" isCollection
# Should see credit_balance fields for users with credits
jsonpath "$.data[?(@.id == '{{user1_id}}')].credit_balance" nth 0 exists
jsonpath "$.data[?(@.id == '{{user2_id}}')].credit_balance" nth 0 exists

# User2 lists users with multiple includes - should succeed
GET http://localhost:3001/admin/api/v1/users?include=groups,billing
[Cookies]
dwctl_session: {{user2_jwt}}
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

# User2 (PlatformManager) gets user1's data - should succeed
GET http://localhost:3001/admin/api/v1/users/{{user1_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user1_id}}"

# User2 gets their own data - should succeed
GET http://localhost:3001/admin/api/v1/users/{{user2_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user2_id}}"

# User2 uses /users/current - should succeed
GET http://localhost:3001/admin/api/v1/users/current
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user2_id}}"

# User2 gets admin's data - should succeed
GET http://localhost:3001/admin/api/v1/users/{{admin_user_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{admin_user_id}}"

# -----------------------------------------------------------------------------
# PlatformManager can see ANY user's credits (has Credits::ReadAll)
# -----------------------------------------------------------------------------

# User2 gets user1's data with include=billing - should succeed
GET http://localhost:3001/admin/api/v1/users/{{user1_id}}?include=billing
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user1_id}}"
jsonpath "$.credit_balance" exists
jsonpath "$.credit_balance" == 100.0

# User2 gets their own data with include=billing - should succeed
GET http://localhost:3001/admin/api/v1/users/{{user2_id}}?include=billing
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Asserts]
jsonpath "$.credit_balance" exists
jsonpath "$.credit_balance" == 200.0

# User2 gets admin's data with include=billing - should succeed
GET http://localhost:3001/admin/api/v1/users/{{admin_user_id}}?include=billing
[Cookies]
dwctl_session: {{user2_jwt}}
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

# User2 (PlatformManager) creates a new standard user - should succeed
POST http://localhost:3001/admin/api/v1/users
[Cookies]
dwctl_session: {{user2_jwt}}
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

# User2 creates another user with multiple roles - should succeed
POST http://localhost:3001/admin/api/v1/users
[Cookies]
dwctl_session: {{user2_jwt}}
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
# User Update (PATCH /users/{user_id}) - Requires Users::UpdateAll
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser CANNOT update users (even themselves)
# -----------------------------------------------------------------------------

# User1 tries to update themselves - should be forbidden
PATCH http://localhost:3001/admin/api/v1/users/{{user1_id}}
[Cookies]
dwctl_session: {{user_jwt}}
{
  "display_name": "User1 Updated"
}
HTTP 403

# User1 tries to update their roles - should be forbidden
PATCH http://localhost:3001/admin/api/v1/users/{{user1_id}}
[Cookies]
dwctl_session: {{user_jwt}}
{
  "roles": ["PlatformManager"]
}
HTTP 403

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

# User2 (PlatformManager) updates created user1 - should succeed
PATCH http://localhost:3001/admin/api/v1/users/{{created_user1_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
{
  "display_name": "Updated Display Name"
}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{created_user1_id}}"
jsonpath "$.display_name" == "Updated Display Name"

# User2 updates user roles - should succeed
PATCH http://localhost:3001/admin/api/v1/users/{{created_user2_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
{
  "roles": ["StandardUser", "RequestViewer"]
}
HTTP 200
[Asserts]
jsonpath "$.roles" contains "StandardUser"
jsonpath "$.roles" contains "RequestViewer"

# User2 updates their own user - should succeed
PATCH http://localhost:3001/admin/api/v1/users/{{user2_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
{
  "display_name": "PM Updated Self"
}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user2_id}}"
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

# User2 (PlatformManager) deletes created user1 - should succeed
DELETE http://localhost:3001/admin/api/v1/users/{{created_user1_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 204

# Verify user is deleted - should get 404
GET http://localhost:3001/admin/api/v1/users/{{created_user1_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 404

# User2 deletes created user2 - should succeed
DELETE http://localhost:3001/admin/api/v1/users/{{created_user2_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 204

# -----------------------------------------------------------------------------
# PlatformManager CANNOT delete themselves
# -----------------------------------------------------------------------------

# User2 tries to delete themselves - should get 400 (cannot self-delete)
DELETE http://localhost:3001/admin/api/v1/users/{{user2_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 400

# =============================================================================
# Cleanup: Restore user2 to original roles
# =============================================================================

# Restore user2 to StandardUser + BatchAPIUser
PATCH http://localhost:3001/admin/api/v1/users/{{user2_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "roles": ["StandardUser", "BatchAPIUser"]
}
HTTP 200

# =============================================================================
# Cleanup Complete
# =============================================================================