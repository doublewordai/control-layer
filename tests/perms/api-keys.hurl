# =============================================================================
# API Keys Permissions Test Suite
# =============================================================================
# Tests CRUD operations on /users/{user_id}/api-keys endpoints
# Covers: StandardUser permissions (own resources only) and PlatformManager 
# permissions (own resources only)
#
# Test users:
# - admin_email: trusted SSO administrator
# - user_jwt: Standard User 1 (user@example.org)
# - user2_jwt: Standard User 2 (user2@example.org)
# =============================================================================

# -----------------------------------------------------------------------------
# Setup: Get user IDs for all test users
# -----------------------------------------------------------------------------

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
  "purpose": "realtime"
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
  "purpose": "realtime"
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
  "purpose": "realtime"
}
HTTP 403

POST http://localhost:3001/admin/api/v1/users/{{user1_id}}/api-keys
[Cookies]
dwctl_session: {{user2_jwt}}
{
  "name": "Should Fail",
  "description": "User2 trying to create key for User1",
  "purpose": "realtime"
}
HTTP 403

# -----------------------------------------------------------------------------
# Platform manager can create API keys for themselves
# -----------------------------------------------------------------------------

POST http://localhost:3001/admin/api/v1/users/{{admin_user_id}}/api-keys
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
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
# Platform managers cannot create customer keys; owners can
# -----------------------------------------------------------------------------

# Staff cannot manage another account's credentials.
POST http://localhost:3001/admin/api/v1/users/{{user1_id}}/api-keys
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
{
  "name": "User1 Second Key",
  "description": "Second key for user1",
  "purpose": "realtime"
}
HTTP 403

POST http://localhost:3001/admin/api/v1/users/{{user1_id}}/api-keys
[Cookies]
dwctl_session: {{user_jwt}}
{
  "name": "User1 Second Key",
  "description": "Second key for user1",
  "purpose": "realtime"
}
HTTP 201
[Captures]
user1_second_key_id: jsonpath "$.id"
user1_second_key_secret: jsonpath "$.key"
[Asserts]
jsonpath "$.name" == "User1 Second Key"
jsonpath "$.user_id" == "{{user1_id}}"

# Staff cannot manage another account's credentials.
POST http://localhost:3001/admin/api/v1/users/{{user2_id}}/api-keys
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
{
  "name": "User2 Second Key",
  "description": "Second key for user2",
  "purpose": "realtime"
}
HTTP 403

POST http://localhost:3001/admin/api/v1/users/{{user2_id}}/api-keys
[Cookies]
dwctl_session: {{user2_jwt}}
{
  "name": "User2 Second Key",
  "description": "Second key for user2",
  "purpose": "realtime"
}
HTTP 201
[Captures]
user2_second_key_id: jsonpath "$.id"
user2_second_key_secret: jsonpath "$.key"
[Asserts]
jsonpath "$.name" == "User2 Second Key"
jsonpath "$.user_id" == "{{user2_id}}"

# =============================================================================
# Purpose Validation Tests - Batch and Playground are system-only
# =============================================================================

# -----------------------------------------------------------------------------
# Standard users CANNOT create batch or playground keys
# -----------------------------------------------------------------------------

# User1 tries to create a batch key - should be rejected
POST http://localhost:3001/admin/api/v1/users/{{user1_id}}/api-keys
[Cookies]
dwctl_session: {{user_jwt}}
{
  "name": "Invalid Batch Key",
  "description": "Should not be allowed",
  "purpose": "batch"
}
HTTP 400
[Asserts]
body contains "batch"
body contains "internal system use"

# User1 tries to create a playground key - should be rejected
POST http://localhost:3001/admin/api/v1/users/{{user1_id}}/api-keys
[Cookies]
dwctl_session: {{user_jwt}}
{
  "name": "Invalid Playground Key",
  "description": "Should not be allowed",
  "purpose": "playground"
}
HTTP 400
[Asserts]
body contains "playground"
body contains "internal system use"

# User2 tries to create a batch key - should be rejected
POST http://localhost:3001/admin/api/v1/users/{{user2_id}}/api-keys
[Cookies]
dwctl_session: {{user2_jwt}}
{
  "name": "Invalid Batch Key User2",
  "description": "Should not be allowed",
  "purpose": "batch"
}
HTTP 400
[Asserts]
body contains "batch"
body contains "internal system use"

# User2 tries to create a playground key - should be rejected
POST http://localhost:3001/admin/api/v1/users/{{user2_id}}/api-keys
[Cookies]
dwctl_session: {{user2_jwt}}
{
  "name": "Invalid Playground Key User2",
  "description": "Should not be allowed",
  "purpose": "playground"
}
HTTP 400
[Asserts]
body contains "playground"
body contains "internal system use"

# -----------------------------------------------------------------------------
# Platform managers ALSO CANNOT create batch or playground keys via API
# -----------------------------------------------------------------------------

# Admin tries to create a batch key for themselves - should be rejected
POST http://localhost:3001/admin/api/v1/users/{{admin_user_id}}/api-keys
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
{
  "name": "Admin Invalid Batch Key",
  "description": "Even admins can't create these",
  "purpose": "batch"
}
HTTP 400
[Asserts]
body contains "batch"
body contains "internal system use"

# Admin tries to create a playground key for themselves - should be rejected
POST http://localhost:3001/admin/api/v1/users/{{admin_user_id}}/api-keys
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
{
  "name": "Admin Invalid Playground Key",
  "description": "Even admins can't create these",
  "purpose": "playground"
}
HTTP 400
[Asserts]
body contains "playground"
body contains "internal system use"

# Admin tries to create a batch key for another user - should be rejected
# Staff cannot manage another account's credentials.
POST http://localhost:3001/admin/api/v1/users/{{user1_id}}/api-keys
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
{
  "name": "Admin Creating Batch Key for User1",
  "description": "Should not be allowed even for admins",
  "purpose": "batch"
}
HTTP 403

# Admin tries to create a playground key for another user - should be rejected
POST http://localhost:3001/admin/api/v1/users/{{user1_id}}/api-keys
[Cookies]
dwctl_session: {{user_jwt}}
{
  "name": "Admin Creating Batch Key for User1",
  "description": "Should not be allowed even for admins",
  "purpose": "batch"
}
HTTP 400
[Asserts]
body contains "batch"
body contains "internal system use"

# Admin tries to create a playground key for another user - should be rejected
# Staff cannot manage another account's credentials.
POST http://localhost:3001/admin/api/v1/users/{{user1_id}}/api-keys
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
{
  "name": "Admin Creating Playground Key for User1",
  "description": "Should not be allowed even for admins",
  "purpose": "playground"
}
HTTP 403

# =============================================================================
# GET /users/{user_id}/api-keys - List API Keys
# =============================================================================

# -----------------------------------------------------------------------------
# Normal users see only their own keys (self-created + owner-created for them)
# -----------------------------------------------------------------------------

POST http://localhost:3001/admin/api/v1/users/{{user1_id}}/api-keys
[Cookies]
dwctl_session: {{user_jwt}}
{
  "name": "Admin Creating Playground Key for User1",
  "description": "Should not be allowed even for admins",
  "purpose": "playground"
}
HTTP 400
[Asserts]
body contains "playground"
body contains "internal system use"

# =============================================================================
# GET /users/{user_id}/api-keys - List API Keys
# =============================================================================

# -----------------------------------------------------------------------------
# Normal users see only their own keys (self-created + owner-created for them)
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
jsonpath "$.data[*].name" contains "User1 Second Key"

GET http://localhost:3001/admin/api/v1/users/{{user2_id}}/api-keys
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data" count == 2
jsonpath "$.total_count" == 2
jsonpath "$.data[*].user_id" contains "{{user2_id}}"
jsonpath "$.data[*].name" contains "User2 Self-Created Key"
jsonpath "$.data[*].name" contains "User2 Second Key"

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
# Platform managers can list only their own keys
# -----------------------------------------------------------------------------

GET http://localhost:3001/admin/api/v1/users/{{admin_user_id}}/api-keys
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
HTTP 200
[Asserts]
jsonpath "$.data" count == 1
jsonpath "$.total_count" == 1
jsonpath "$.data[0].name" == "Admin Self-Created Key"
jsonpath "$.data[0].user_id" == "{{admin_user_id}}"

GET http://localhost:3001/admin/api/v1/users/{{user1_id}}/api-keys
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
HTTP 403

GET http://localhost:3001/admin/api/v1/users/{{user2_id}}/api-keys
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
HTTP 403

# =============================================================================
# GET /users/{user_id}/api-keys/{id} - Get Specific API Key
# =============================================================================

# -----------------------------------------------------------------------------
# Normal users can get their own API keys (self-created and owner-created)
# -----------------------------------------------------------------------------

GET http://localhost:3001/admin/api/v1/users/{{user1_id}}/api-keys/{{user1_own_key_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user1_own_key_id}}"
jsonpath "$.name" == "User1 Self-Created Key"
jsonpath "$.user_id" == "{{user1_id}}"

GET http://localhost:3001/admin/api/v1/users/{{user1_id}}/api-keys/{{user1_second_key_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user1_second_key_id}}"
jsonpath "$.name" == "User1 Second Key"
jsonpath "$.user_id" == "{{user1_id}}"

GET http://localhost:3001/admin/api/v1/users/{{user2_id}}/api-keys/{{user2_own_key_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user2_own_key_id}}"
jsonpath "$.name" == "User2 Self-Created Key"

GET http://localhost:3001/admin/api/v1/users/{{user2_id}}/api-keys/{{user2_second_key_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user2_second_key_id}}"
jsonpath "$.name" == "User2 Second Key"

# -----------------------------------------------------------------------------
# Normal users CANNOT get other users' API keys
# -----------------------------------------------------------------------------

GET http://localhost:3001/admin/api/v1/users/{{user2_id}}/api-keys/{{user2_own_key_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

GET http://localhost:3001/admin/api/v1/users/{{user2_id}}/api-keys/{{user2_second_key_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

GET http://localhost:3001/admin/api/v1/users/{{user1_id}}/api-keys/{{user1_own_key_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 403

GET http://localhost:3001/admin/api/v1/users/{{user1_id}}/api-keys/{{user1_second_key_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 403

# -----------------------------------------------------------------------------
# Platform managers can read only their own keys
# -----------------------------------------------------------------------------

GET http://localhost:3001/admin/api/v1/users/{{admin_user_id}}/api-keys/{{admin_own_key_id}}
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{admin_own_key_id}}"
jsonpath "$.name" == "Admin Self-Created Key"

GET http://localhost:3001/admin/api/v1/users/{{user1_id}}/api-keys/{{user1_own_key_id}}
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
HTTP 403

GET http://localhost:3001/admin/api/v1/users/{{user1_id}}/api-keys/{{user1_second_key_id}}
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
HTTP 403

GET http://localhost:3001/admin/api/v1/users/{{user2_id}}/api-keys/{{user2_own_key_id}}
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
HTTP 403

GET http://localhost:3001/admin/api/v1/users/{{user2_id}}/api-keys/{{user2_second_key_id}}
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
HTTP 403

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

DELETE http://localhost:3001/admin/api/v1/users/{{user2_id}}/api-keys/{{user2_second_key_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

DELETE http://localhost:3001/admin/api/v1/users/{{user1_id}}/api-keys/{{user1_own_key_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 403

DELETE http://localhost:3001/admin/api/v1/users/{{user1_id}}/api-keys/{{user1_second_key_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 403

# -----------------------------------------------------------------------------
# Normal users CAN delete their own keys (self-created and owner-created)
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

DELETE http://localhost:3001/admin/api/v1/users/{{user1_id}}/api-keys/{{user1_second_key_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 204

# Verify it's deleted
GET http://localhost:3001/admin/api/v1/users/{{user1_id}}/api-keys/{{user1_second_key_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 404

# -----------------------------------------------------------------------------
# Platform managers can delete only their own keys; customers delete theirs
# -----------------------------------------------------------------------------

DELETE http://localhost:3001/admin/api/v1/users/{{admin_user_id}}/api-keys/{{admin_own_key_id}}
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
HTTP 204

# Verify it's deleted
GET http://localhost:3001/admin/api/v1/users/{{admin_user_id}}/api-keys/{{admin_own_key_id}}
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
HTTP 404

# Staff cannot manage another account's credentials.
DELETE http://localhost:3001/admin/api/v1/users/{{user2_id}}/api-keys/{{user2_own_key_id}}
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
HTTP 403

# Verify it's deleted
DELETE http://localhost:3001/admin/api/v1/users/{{user2_id}}/api-keys/{{user2_own_key_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 204

# Verify it's deleted
GET http://localhost:3001/admin/api/v1/users/{{user2_id}}/api-keys/{{user2_own_key_id}}
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
HTTP 403

# Staff cannot manage another account's credentials.
DELETE http://localhost:3001/admin/api/v1/users/{{user2_id}}/api-keys/{{user2_second_key_id}}
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
HTTP 403

# Verify it's deleted
DELETE http://localhost:3001/admin/api/v1/users/{{user2_id}}/api-keys/{{user2_second_key_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 204

# Verify it's deleted
GET http://localhost:3001/admin/api/v1/users/{{user2_id}}/api-keys/{{user2_second_key_id}}
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
HTTP 403

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
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
HTTP 200
[Asserts]
jsonpath "$.data" count == 0
jsonpath "$.total_count" == 0