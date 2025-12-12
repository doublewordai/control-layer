# =============================================================================
# API Keys Permissions Test Suite
# =============================================================================
# Tests CRUD operations on /users/{user_id}/api-keys endpoints
# Covers: StandardUser permissions (own resources only) and PlatformManager 
# permissions (all resources)
#
# Test users:
# - admin_jwt: Platform Manager
# - user_jwt: Standard User 1 (user@example.org)
# - user2_jwt: Standard User 2 (user2@example.org)
# =============================================================================

# -----------------------------------------------------------------------------
# Setup: Get user IDs for all test users
# -----------------------------------------------------------------------------

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

# =============================================================================
# POST /users/{user_id}/api-keys - Create API Keys
# =============================================================================

# -----------------------------------------------------------------------------
# Normal users can create API keys for themselves
# -----------------------------------------------------------------------------

POST http://localhost:3001/admin/api/v1/users/{{user1_id}}/api-keys
[Cookies]
dwctl_session: {{user_jwt}}
{
  "name": "User1 Self-Created Key",
  "description": "Created by user1 for themselves",
  "purpose": "inference"
}
HTTP 201
[Captures]
user1_own_key_id: jsonpath "$.id"
user1_own_key_secret: jsonpath "$.key"
[Asserts]
jsonpath "$.name" == "User1 Self-Created Key"
jsonpath "$.user_id" == "{{user1_id}}"
jsonpath "$.key" startsWith "sk-"

POST http://localhost:3001/admin/api/v1/users/{{user2_id}}/api-keys
[Cookies]
dwctl_session: {{user2_jwt}}
{
  "name": "User2 Self-Created Key",
  "description": "Created by user2 for themselves",
  "purpose": "inference"
}
HTTP 201
[Captures]
user2_own_key_id: jsonpath "$.id"
user2_own_key_secret: jsonpath "$.key"
[Asserts]
jsonpath "$.name" == "User2 Self-Created Key"
jsonpath "$.user_id" == "{{user2_id}}"

# -----------------------------------------------------------------------------
# Normal users CANNOT create API keys for other users
# -----------------------------------------------------------------------------

POST http://localhost:3001/admin/api/v1/users/{{user2_id}}/api-keys
[Cookies]
dwctl_session: {{user_jwt}}
{
  "name": "Should Fail",
  "description": "User1 trying to create key for User2",
  "purpose": "inference"
}
HTTP 403

POST http://localhost:3001/admin/api/v1/users/{{user1_id}}/api-keys
[Cookies]
dwctl_session: {{user2_jwt}}
{
  "name": "Should Fail",
  "description": "User2 trying to create key for User1",
  "purpose": "inference"
}
HTTP 403

# -----------------------------------------------------------------------------
# Platform manager can create API keys for themselves
# -----------------------------------------------------------------------------

POST http://localhost:3001/admin/api/v1/users/{{admin_user_id}}/api-keys
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "name": "Admin Self-Created Key",
  "description": "Created by admin for themselves",
  "purpose": "platform"
}
HTTP 201
[Captures]
admin_own_key_id: jsonpath "$.id"
admin_own_key_secret: jsonpath "$.key"
[Asserts]
jsonpath "$.name" == "Admin Self-Created Key"
jsonpath "$.user_id" == "{{admin_user_id}}"

# -----------------------------------------------------------------------------
# Platform manager can create API keys for normal users
# -----------------------------------------------------------------------------

POST http://localhost:3001/admin/api/v1/users/{{user1_id}}/api-keys
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "name": "User1 Admin-Created Key",
  "description": "Created by admin for user1",
  "purpose": "inference"
}
HTTP 201
[Captures]
user1_admin_created_key_id: jsonpath "$.id"
user1_admin_created_key_secret: jsonpath "$.key"
[Asserts]
jsonpath "$.name" == "User1 Admin-Created Key"
jsonpath "$.user_id" == "{{user1_id}}"

POST http://localhost:3001/admin/api/v1/users/{{user2_id}}/api-keys
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "name": "User2 Admin-Created Key",
  "description": "Created by admin for user2",
  "purpose": "inference"
}
HTTP 201
[Captures]
user2_admin_created_key_id: jsonpath "$.id"
user2_admin_created_key_secret: jsonpath "$.key"
[Asserts]
jsonpath "$.name" == "User2 Admin-Created Key"
jsonpath "$.user_id" == "{{user2_id}}"

# =============================================================================
# GET /users/{user_id}/api-keys - List API Keys
# =============================================================================

# -----------------------------------------------------------------------------
# Normal users see only their own keys (self-created + admin-created for them)
# -----------------------------------------------------------------------------

GET http://localhost:3001/admin/api/v1/users/{{user1_id}}/api-keys
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data" count == 2
jsonpath "$.total_count" == 2
jsonpath "$.data[*].user_id" contains "{{user1_id}}"
jsonpath "$.data[*].name" contains "User1 Self-Created Key"
jsonpath "$.data[*].name" contains "User1 Admin-Created Key"

GET http://localhost:3001/admin/api/v1/users/{{user2_id}}/api-keys
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data" count == 2
jsonpath "$.total_count" == 2
jsonpath "$.data[*].user_id" contains "{{user2_id}}"
jsonpath "$.data[*].name" contains "User2 Self-Created Key"
jsonpath "$.data[*].name" contains "User2 Admin-Created Key"

# -----------------------------------------------------------------------------
# Normal users CANNOT list other users' API keys
# -----------------------------------------------------------------------------

GET http://localhost:3001/admin/api/v1/users/{{user2_id}}/api-keys
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

GET http://localhost:3001/admin/api/v1/users/{{admin_user_id}}/api-keys
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

GET http://localhost:3001/admin/api/v1/users/{{user1_id}}/api-keys
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 403

GET http://localhost:3001/admin/api/v1/users/{{admin_user_id}}/api-keys
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 403

# -----------------------------------------------------------------------------
# Platform manager can list API keys for any user
# -----------------------------------------------------------------------------

GET http://localhost:3001/admin/api/v1/users/{{admin_user_id}}/api-keys
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data" count == 1
jsonpath "$.total_count" == 1
jsonpath "$.data[0].name" == "Admin Self-Created Key"
jsonpath "$.data[0].user_id" == "{{admin_user_id}}"

GET http://localhost:3001/admin/api/v1/users/{{user1_id}}/api-keys
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data" count == 2
jsonpath "$.total_count" == 2
jsonpath "$.data[*].user_id" contains "{{user1_id}}"

GET http://localhost:3001/admin/api/v1/users/{{user2_id}}/api-keys
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data" count == 2
jsonpath "$.total_count" == 2
jsonpath "$.data[*].user_id" contains "{{user2_id}}"

# =============================================================================
# GET /users/{user_id}/api-keys/{id} - Get Specific API Key
# =============================================================================

# -----------------------------------------------------------------------------
# Normal users can get their own API keys (self-created and admin-created)
# -----------------------------------------------------------------------------

GET http://localhost:3001/admin/api/v1/users/{{user1_id}}/api-keys/{{user1_own_key_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user1_own_key_id}}"
jsonpath "$.name" == "User1 Self-Created Key"
jsonpath "$.user_id" == "{{user1_id}}"

GET http://localhost:3001/admin/api/v1/users/{{user1_id}}/api-keys/{{user1_admin_created_key_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user1_admin_created_key_id}}"
jsonpath "$.name" == "User1 Admin-Created Key"
jsonpath "$.user_id" == "{{user1_id}}"

GET http://localhost:3001/admin/api/v1/users/{{user2_id}}/api-keys/{{user2_own_key_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user2_own_key_id}}"
jsonpath "$.name" == "User2 Self-Created Key"

GET http://localhost:3001/admin/api/v1/users/{{user2_id}}/api-keys/{{user2_admin_created_key_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user2_admin_created_key_id}}"
jsonpath "$.name" == "User2 Admin-Created Key"

# -----------------------------------------------------------------------------
# Normal users CANNOT get other users' API keys
# -----------------------------------------------------------------------------

GET http://localhost:3001/admin/api/v1/users/{{user2_id}}/api-keys/{{user2_own_key_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

GET http://localhost:3001/admin/api/v1/users/{{user2_id}}/api-keys/{{user2_admin_created_key_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

GET http://localhost:3001/admin/api/v1/users/{{user1_id}}/api-keys/{{user1_own_key_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 403

GET http://localhost:3001/admin/api/v1/users/{{user1_id}}/api-keys/{{user1_admin_created_key_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 403

# -----------------------------------------------------------------------------
# Platform manager can get any API key (own, user-created, admin-created)
# -----------------------------------------------------------------------------

GET http://localhost:3001/admin/api/v1/users/{{admin_user_id}}/api-keys/{{admin_own_key_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{admin_own_key_id}}"
jsonpath "$.name" == "Admin Self-Created Key"

GET http://localhost:3001/admin/api/v1/users/{{user1_id}}/api-keys/{{user1_own_key_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user1_own_key_id}}"
jsonpath "$.name" == "User1 Self-Created Key"

GET http://localhost:3001/admin/api/v1/users/{{user1_id}}/api-keys/{{user1_admin_created_key_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user1_admin_created_key_id}}"
jsonpath "$.name" == "User1 Admin-Created Key"

GET http://localhost:3001/admin/api/v1/users/{{user2_id}}/api-keys/{{user2_own_key_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user2_own_key_id}}"
jsonpath "$.name" == "User2 Self-Created Key"

GET http://localhost:3001/admin/api/v1/users/{{user2_id}}/api-keys/{{user2_admin_created_key_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user2_admin_created_key_id}}"
jsonpath "$.name" == "User2 Admin-Created Key"

# =============================================================================
# DELETE /users/{user_id}/api-keys/{id} - Delete API Keys
# =============================================================================

# -----------------------------------------------------------------------------
# Test blocked scenarios first (no keys deleted)
# -----------------------------------------------------------------------------

# Normal users CANNOT delete other users' keys
DELETE http://localhost:3001/admin/api/v1/users/{{user2_id}}/api-keys/{{user2_own_key_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

DELETE http://localhost:3001/admin/api/v1/users/{{user2_id}}/api-keys/{{user2_admin_created_key_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

DELETE http://localhost:3001/admin/api/v1/users/{{user1_id}}/api-keys/{{user1_own_key_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 403

DELETE http://localhost:3001/admin/api/v1/users/{{user1_id}}/api-keys/{{user1_admin_created_key_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 403

# -----------------------------------------------------------------------------
# Normal users CAN delete their own keys (self-created and admin-created)
# -----------------------------------------------------------------------------

DELETE http://localhost:3001/admin/api/v1/users/{{user1_id}}/api-keys/{{user1_own_key_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 204

# Verify it's deleted
GET http://localhost:3001/admin/api/v1/users/{{user1_id}}/api-keys/{{user1_own_key_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 404

DELETE http://localhost:3001/admin/api/v1/users/{{user1_id}}/api-keys/{{user1_admin_created_key_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 204

# Verify it's deleted
GET http://localhost:3001/admin/api/v1/users/{{user1_id}}/api-keys/{{user1_admin_created_key_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 404

# -----------------------------------------------------------------------------
# Platform manager CAN delete any key (own, user-created, admin-created)
# -----------------------------------------------------------------------------

DELETE http://localhost:3001/admin/api/v1/users/{{admin_user_id}}/api-keys/{{admin_own_key_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# Verify it's deleted
GET http://localhost:3001/admin/api/v1/users/{{admin_user_id}}/api-keys/{{admin_own_key_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 404

DELETE http://localhost:3001/admin/api/v1/users/{{user2_id}}/api-keys/{{user2_own_key_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# Verify it's deleted
GET http://localhost:3001/admin/api/v1/users/{{user2_id}}/api-keys/{{user2_own_key_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 404

DELETE http://localhost:3001/admin/api/v1/users/{{user2_id}}/api-keys/{{user2_admin_created_key_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# Verify it's deleted
GET http://localhost:3001/admin/api/v1/users/{{user2_id}}/api-keys/{{user2_admin_created_key_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 404

# -----------------------------------------------------------------------------
# Final verification: All keys should be deleted
# -----------------------------------------------------------------------------

GET http://localhost:3001/admin/api/v1/users/{{user1_id}}/api-keys
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data" count == 0
jsonpath "$.total_count" == 0

GET http://localhost:3001/admin/api/v1/users/{{user2_id}}/api-keys
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data" count == 0
jsonpath "$.total_count" == 0

GET http://localhost:3001/admin/api/v1/users/{{admin_user_id}}/api-keys
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data" count == 0
jsonpath "$.total_count" == 0