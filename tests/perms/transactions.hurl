# =============================================================================
# Transactions Permissions Test Suite
# =============================================================================
# Tests all CRUD operations on /admin/api/v1/transactions endpoints
#
# Permission model:
# - StandardUser: Can only see their own transactions
#   - Cannot create transactions (no Credits::CreateAll)
#   - Can read own transactions (has Credits::ReadOwn)
#   - Cannot use all=true or user_id for other users
# - PlatformManager: Full access to credit system
#   - Can create transactions for any user (has Credits::CreateAll)
#   - Can read all transactions (has Credits::ReadAll)
#   - Can use all=true and user_id parameters
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

# =============================================================================
# Transaction Creation (POST /transactions) - Requires Credits::CreateAll
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser CANNOT create transactions
# -----------------------------------------------------------------------------

# User1 tries to grant credits to themselves - should be forbidden
POST http://localhost:3001/admin/api/v1/transactions
[Cookies]
dwctl_session: {{user_jwt}}
{
  "user_id": "{{user1_id}}",
  "transaction_type": "admin_grant",
  "amount": "100.0",
  "source_id": "test-grant-1",
  "description": "Self grant attempt"
}
HTTP 403

# User1 tries to grant credits to another user - should be forbidden
POST http://localhost:3001/admin/api/v1/transactions
[Cookies]
dwctl_session: {{user_jwt}}
{
  "user_id": "{{user2_id}}",
  "transaction_type": "admin_grant",
  "amount": "50.0",
  "source_id": "test-grant-2",
  "description": "Grant to other user"
}
HTTP 403

# User1 tries to remove credits - should be forbidden
POST http://localhost:3001/admin/api/v1/transactions
[Cookies]
dwctl_session: {{user_jwt}}
{
  "user_id": "{{user1_id}}",
  "transaction_type": "admin_removal",
  "amount": "10.0",
  "source_id": "test-removal-1",
  "description": "Remove credits"
}
HTTP 403

# -----------------------------------------------------------------------------
# PlatformManager CAN create transactions (has Credits::CreateAll)
# -----------------------------------------------------------------------------

# User2 (PlatformManager) grants credits to user1 - should succeed
POST http://localhost:3001/admin/api/v1/transactions
[Cookies]
dwctl_session: {{user2_jwt}}
{
  "user_id": "{{user1_id}}",
  "transaction_type": "admin_grant",
  "amount": "100.0",
  "source_id": "pm-grant-to-user1",
  "description": "PM grants credits to user1"
}
HTTP 201
[Asserts]
jsonpath "$.id" exists
jsonpath "$.user_id" == "{{user1_id}}"
jsonpath "$.transaction_type" == "admin_grant"
jsonpath "$.amount" matches "^100(\\.0+)?$"
jsonpath "$.description" == "PM grants credits to user1"
[Captures]
user1_transaction_id: jsonpath "$.id"

# User2 (PlatformManager) grants credits to themselves - should succeed
POST http://localhost:3001/admin/api/v1/transactions
[Cookies]
dwctl_session: {{user2_jwt}}
{
  "user_id": "{{user2_id}}",
  "transaction_type": "admin_grant",
  "amount": "200.0",
  "source_id": "pm-grant-to-self",
  "description": "PM grants credits to themselves"
}
HTTP 201
[Asserts]
jsonpath "$.user_id" == "{{user2_id}}"
jsonpath "$.amount" matches "^200(\\.0+)?$"
[Captures]
user2_transaction_id: jsonpath "$.id"

# User2 (PlatformManager) creates another transaction for user1
POST http://localhost:3001/admin/api/v1/transactions
[Cookies]
dwctl_session: {{user2_jwt}}
{
  "user_id": "{{user1_id}}",
  "transaction_type": "admin_grant",
  "amount": "50.0",
  "source_id": "pm-grant-2",
  "description": "Second grant to user1"
}
HTTP 201
[Captures]
user1_transaction2_id: jsonpath "$.id"

# =============================================================================
# Transaction Retrieval by ID (GET /transactions/{id})
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser can ONLY get their own transactions
# -----------------------------------------------------------------------------

# User1 gets their own transaction - should succeed
GET http://localhost:3001/admin/api/v1/transactions/{{user1_transaction_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user1_transaction_id}}"
jsonpath "$.user_id" == "{{user1_id}}"
jsonpath "$.amount" matches "^100(\\.0+)?$"

# User1 tries to get user2's transaction - should get 404 (not 403, for security)
GET http://localhost:3001/admin/api/v1/transactions/{{user2_transaction_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 404

# -----------------------------------------------------------------------------
# PlatformManager can get ANY transaction (has Credits::ReadAll)
# -----------------------------------------------------------------------------

# User2 (PlatformManager) gets user1's transaction - should succeed
GET http://localhost:3001/admin/api/v1/transactions/{{user1_transaction_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user1_transaction_id}}"
jsonpath "$.user_id" == "{{user1_id}}"

# User2 gets their own transaction - should succeed
GET http://localhost:3001/admin/api/v1/transactions/{{user2_transaction_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user2_transaction_id}}"
jsonpath "$.user_id" == "{{user2_id}}"

# =============================================================================
# Transaction Listing (GET /transactions)
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser: Default behavior (no params) - only own transactions
# -----------------------------------------------------------------------------

# User1 lists transactions without params - should only see own
GET http://localhost:3001/admin/api/v1/transactions
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$" isCollection
# Should only see user1's transactions
jsonpath "$[*].user_id" includes "{{user1_id}}"
# Should NOT see user2's transactions (verify no user2_id in results)
jsonpath "$[?(@.user_id == '{{user2_id}}')]" count == 0

# -----------------------------------------------------------------------------
# StandardUser CANNOT use all=true (requires Credits::ReadAll)
# -----------------------------------------------------------------------------

# User1 tries to list all transactions - should be forbidden
GET http://localhost:3001/admin/api/v1/transactions?all=true
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# -----------------------------------------------------------------------------
# StandardUser CANNOT filter by another user_id
# -----------------------------------------------------------------------------

# User1 tries to get user2's transactions - should be forbidden
GET http://localhost:3001/admin/api/v1/transactions?user_id={{user2_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User1 tries to get their own transactions via user_id param - should succeed (allowed to specify own ID)
GET http://localhost:3001/admin/api/v1/transactions?user_id={{user1_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$[*].user_id" includes "{{user1_id}}"
jsonpath "$[?(@.user_id == '{{user2_id}}')]" count == 0

# -----------------------------------------------------------------------------
# StandardUser CANNOT circumvent scope with query param combinations
# -----------------------------------------------------------------------------

# User1 tries all=true with user_id (all takes precedence) - should be forbidden
GET http://localhost:3001/admin/api/v1/transactions?all=true&user_id={{user1_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User1 tries all=true with pagination - should be forbidden
GET http://localhost:3001/admin/api/v1/transactions?all=true&skip=0&limit=10
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User1 tries other user_id with pagination - should be forbidden
GET http://localhost:3001/admin/api/v1/transactions?user_id={{user2_id}}&skip=0&limit=10
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User1 tries all combinations that should fail - should be forbidden
GET http://localhost:3001/admin/api/v1/transactions?all=true&user_id={{user2_id}}&skip=0&limit=5
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# -----------------------------------------------------------------------------
# PlatformManager: Default behavior (no params) - only own transactions
# -----------------------------------------------------------------------------

# User2 (PlatformManager) lists without params - should only see own by default
GET http://localhost:3001/admin/api/v1/transactions
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Asserts]
jsonpath "$" isCollection
# Should only see user2's transactions by default
jsonpath "$[*].user_id" includes "{{user2_id}}"
# Should NOT see user1's transactions without explicit all=true
jsonpath "$[?(@.user_id == '{{user1_id}}')]" count == 0

# -----------------------------------------------------------------------------
# PlatformManager CAN use all=true (has Credits::ReadAll)
# -----------------------------------------------------------------------------

# User2 lists all transactions - should see everyone's
GET http://localhost:3001/admin/api/v1/transactions?all=true
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Asserts]
jsonpath "$" isCollection
# Should see both user1's and user2's transactions
jsonpath "$[*].user_id" includes "{{user1_id}}"
jsonpath "$[*].user_id" includes "{{user2_id}}"

# User2 uses all=true with pagination - should succeed
GET http://localhost:3001/admin/api/v1/transactions?all=true&skip=0&limit=10
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Asserts]
jsonpath "$" isCollection

# -----------------------------------------------------------------------------
# PlatformManager CAN filter by specific user_id
# -----------------------------------------------------------------------------

# User2 gets user1's transactions specifically - should succeed
GET http://localhost:3001/admin/api/v1/transactions?user_id={{user1_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Asserts]
jsonpath "$" isCollection
# Should only see user1's transactions
jsonpath "$[*].user_id" includes "{{user1_id}}"
# Should NOT see user2's transactions
jsonpath "$[?(@.user_id == '{{user2_id}}')]" count == 0

# User2 gets their own transactions via user_id - should succeed
GET http://localhost:3001/admin/api/v1/transactions?user_id={{user2_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Asserts]
jsonpath "$[*].user_id" includes "{{user2_id}}"
jsonpath "$[?(@.user_id == '{{user1_id}}')]" count == 0

# User2 uses user_id with pagination - should succeed
GET http://localhost:3001/admin/api/v1/transactions?user_id={{user1_id}}&skip=0&limit=5
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Asserts]
jsonpath "$" isCollection

# -----------------------------------------------------------------------------
# Verify all=true takes precedence over user_id (returns all, not filtered)
# -----------------------------------------------------------------------------

# User2 uses all=true with user_id - all takes precedence, should see everyone
GET http://localhost:3001/admin/api/v1/transactions?all=true&user_id={{user1_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Asserts]
jsonpath "$" isCollection
# Should see BOTH users despite user_id filter (all=true takes precedence)
jsonpath "$[*].user_id" includes "{{user1_id}}"
jsonpath "$[*].user_id" includes "{{user2_id}}"

# User2 uses all=true with user_id and pagination - should see everyone
GET http://localhost:3001/admin/api/v1/transactions?all=true&user_id={{user2_id}}&skip=0&limit=10
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Asserts]
jsonpath "$" isCollection

# =============================================================================
# Pagination Testing
# =============================================================================

# User2 (PlatformManager) tests pagination on all transactions
GET http://localhost:3001/admin/api/v1/transactions?all=true&skip=0&limit=1
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Asserts]
jsonpath "$" isCollection
jsonpath "$" count <= 1

# User1 tests pagination on own transactions
GET http://localhost:3001/admin/api/v1/transactions?skip=0&limit=1
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$" isCollection
jsonpath "$" count <= 1
# All results should be for user1
jsonpath "$[*].user_id" includes "{{user1_id}}"

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