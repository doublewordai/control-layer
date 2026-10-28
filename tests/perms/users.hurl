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
# - Admin/PlatformManager: Read-only customer management plus enable-only ZDR
#   - Can list all users (has Users::ReadAll)
#   - Can read any user (has Users::ReadAll)
#   - Can see any user's credits with include=billing (has Credits::ReadAll)
#   - Cannot create, change roles, or delete other users
#
# Note: Users created via registration get StandardUser, BatchAPIUser, and
# BackgroundInferenceUser roles by default
# =============================================================================

# -----------------------------------------------------------------------------
# Setup: Get user IDs
# -----------------------------------------------------------------------------

# Get user IDs
GET http://localhost:3001/admin/api/v1/users/current
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
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

# Grant some credits to user1 for billing tests
POST http://localhost:3001/admin/api/v1/transactions
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
{
  "user_id": "{{user1_id}}",
  "transaction_type": "admin_grant",
  "amount": "100.0",
  "source_id": "test-grant-user1",
  "description": "Credits for testing"
}
HTTP 201

# =============================================================================
# Unauthenticated Access - All operations should be forbidden
# =============================================================================

# Unauthenticated user tries to list users
GET http://localhost:3001/admin/api/v1/users
HTTP 401

# Unauthenticated user tries to get a user
GET http://localhost:3001/admin/api/v1/users/{{user1_id}}
HTTP 401

# Unauthenticated user tries to create a user
POST http://localhost:3001/admin/api/v1/users
{
  "username": "hacker",
  "email": "hacker@example.com",
  "roles": ["PlatformManager"]
}
HTTP 401

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
# Admin CAN list users (has Users::ReadAll)
# -----------------------------------------------------------------------------

# Admin lists users - should succeed
GET http://localhost:3001/admin/api/v1/users
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
HTTP 200
[Asserts]
jsonpath "$.data" isCollection
jsonpath "$.total_count" exists
# Should see multiple users in the system
jsonpath "$.data[*].id" contains "{{user1_id}}"
jsonpath "$.data[*].id" contains "{{admin_user_id}}"

# Admin lists users with pagination - should succeed
GET http://localhost:3001/admin/api/v1/users?skip=0&limit=5
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
HTTP 200
[Asserts]
jsonpath "$.data" isCollection

# Admin lists users with include=groups - should succeed
GET http://localhost:3001/admin/api/v1/users?include=groups
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
HTTP 200
[Asserts]
jsonpath "$.data" isCollection
# Groups may or may not be present, but should not error

# Admin lists users with include=billing - should succeed (has Credits::ReadAll)
GET http://localhost:3001/admin/api/v1/users?include=billing
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
HTTP 200
[Asserts]
jsonpath "$.data" isCollection
# Should see credit_balance fields for users with credits
jsonpath "$.data[?(@.id == '{{user1_id}}')].credit_balance" nth 0 exists

# Admin lists users with multiple includes - should succeed
GET http://localhost:3001/admin/api/v1/users?include=groups,billing
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
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
jsonpath "$.credit_balance" >= 100.0

# User1 uses /users/current with include=billing - should succeed
GET http://localhost:3001/admin/api/v1/users/current?include=billing
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$.credit_balance" exists

# User1 tries to get user2's billing - should be forbidden
GET http://localhost:3001/admin/api/v1/users/{{user2_id}}?include=billing
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# -----------------------------------------------------------------------------
# Admin can read ANY user (has Users::ReadAll)
# -----------------------------------------------------------------------------

# Admin gets user1's data - should succeed
GET http://localhost:3001/admin/api/v1/users/{{user1_id}}
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user1_id}}"

# Admin gets their own data - should succeed
GET http://localhost:3001/admin/api/v1/users/{{admin_user_id}}
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{admin_user_id}}"

# Admin uses /users/current - should succeed
GET http://localhost:3001/admin/api/v1/users/current
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{admin_user_id}}"

# Admin gets user2's data - should succeed
GET http://localhost:3001/admin/api/v1/users/{{user2_id}}
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user2_id}}"

# -----------------------------------------------------------------------------
# Admin can see ANY user's credits (has Credits::ReadAll)
# -----------------------------------------------------------------------------

# Admin gets user1's data with include=billing - should succeed
GET http://localhost:3001/admin/api/v1/users/{{user1_id}}?include=billing
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user1_id}}"
jsonpath "$.credit_balance" exists
jsonpath "$.credit_balance" >= 100.0

# Admin gets their own data with include=billing - should succeed
GET http://localhost:3001/admin/api/v1/users/{{admin_user_id}}?include=billing
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
HTTP 200
[Asserts]
jsonpath "$.credit_balance" exists

# Admin gets user2's data with include=billing - should succeed
GET http://localhost:3001/admin/api/v1/users/{{user2_id}}?include=billing
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user2_id}}"
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
# SSO admin CANNOT create users
# -----------------------------------------------------------------------------

# Admin cannot create a customer identity
POST http://localhost:3001/admin/api/v1/users
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
{
  "username": "created-user-1",
  "email": "createduser1@example.com",
  "display_name": "Created User 1",
  "roles": ["StandardUser"]
}
HTTP 403

# Admin cannot create a privileged customer identity
POST http://localhost:3001/admin/api/v1/users
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
{
  "username": "created-user-2",
  "email": "createduser2@example.com",
  "roles": ["StandardUser", "BatchAPIUser"]
}
HTTP 403

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

# Verify user1 still only has their configured default roles
GET http://localhost:3001/admin/api/v1/users/{{user1_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$.roles" count == 3
jsonpath "$.roles" contains "StandardUser"
jsonpath "$.roles" contains "BatchAPIUser"
jsonpath "$.roles" contains "BackgroundInferenceUser"
jsonpath "$.roles" not contains "PlatformManager"

# User1 tries to remove their StandardUser role - should be forbidden
PATCH http://localhost:3001/admin/api/v1/users/{{user1_id}}
[Cookies]
dwctl_session: {{user_jwt}}
{
  "roles": []
}
HTTP 403

# User1 tries to add RequestViewer role - should be forbidden
PATCH http://localhost:3001/admin/api/v1/users/{{user1_id}}
[Cookies]
dwctl_session: {{user_jwt}}
{
  "roles": ["StandardUser", "BatchAPIUser", "RequestViewer"]
}
HTTP 403

# User1 tries to update display_name AND roles together - should be forbidden
PATCH http://localhost:3001/admin/api/v1/users/{{user1_id}}
[Cookies]
dwctl_session: {{user_jwt}}
{
  "display_name": "Trying to sneak in roles",
  "roles": ["StandardUser", "PlatformManager"]
}
HTTP 403

# Verify display_name was NOT updated (entire request should be rejected)
GET http://localhost:3001/admin/api/v1/users/{{user1_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$.display_name" == "User1 Updated Display Name"
jsonpath "$.roles" count == 3
jsonpath "$.roles" contains "StandardUser"
jsonpath "$.roles" contains "BatchAPIUser"
jsonpath "$.roles" contains "BackgroundInferenceUser"

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
# SSO admin CANNOT change customer identity or roles
# -----------------------------------------------------------------------------

# Admin cannot change a customer profile
PATCH http://localhost:3001/admin/api/v1/users/{{user1_id}}
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
{
  "display_name": "Updated Display Name"
}
HTTP 403

# Admin cannot change customer roles
PATCH http://localhost:3001/admin/api/v1/users/{{user2_id}}
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
{
  "roles": ["StandardUser", "RequestViewer"]
}
HTTP 403

# Admin updates their own user - should succeed
PATCH http://localhost:3001/admin/api/v1/users/{{admin_user_id}}
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
{
  "display_name": "Admin Updated Self"
}
HTTP 200
[Asserts]
jsonpath "$.display_name" == "Admin Updated Self"

# =============================================================================
# User Deletion (DELETE /users/{user_id}) - Requires Users::DeleteAll
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser CANNOT delete users
# -----------------------------------------------------------------------------

# User1 tries to delete created user - should be forbidden
DELETE http://localhost:3001/admin/api/v1/users/{{user1_id}}
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
# SSO admin CANNOT delete users
# -----------------------------------------------------------------------------

# Admin cannot delete user1
DELETE http://localhost:3001/admin/api/v1/users/{{user1_id}}
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
HTTP 403

# Verify the customer account is still present
GET http://localhost:3001/admin/api/v1/users/{{user1_id}}
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
HTTP 200

# Admin cannot delete user2
DELETE http://localhost:3001/admin/api/v1/users/{{user2_id}}
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
HTTP 403

# Admin cannot delete their own account
DELETE http://localhost:3001/admin/api/v1/users/{{admin_user_id}}
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
HTTP 403

# =============================================================================
# PlatformManager Self-Role-Update Tests
# =============================================================================
# Test that PlatformManager (admin) CANNOT update their own roles
# =============================================================================

# Admin cannot update their own roles
PATCH http://localhost:3001/admin/api/v1/users/{{admin_user_id}}
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
{
  "roles": ["PlatformManager", "BatchAPIUser", "RequestViewer"]
}
HTTP 403

# Verify the account remains readable
GET http://localhost:3001/admin/api/v1/users/{{admin_user_id}}
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
HTTP 200

# A second attempt to set their own roles is also denied
PATCH http://localhost:3001/admin/api/v1/users/{{admin_user_id}}
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
{
  "roles": ["PlatformManager", "BatchAPIUser"]
}
HTTP 403

# Admin can read their own billing data
GET http://localhost:3001/admin/api/v1/users/{{admin_user_id}}?include=billing
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
HTTP 200
[Asserts]
jsonpath "$.credit_balance" exists

# =============================================================================
# Cleanup Complete
# =============================================================================
