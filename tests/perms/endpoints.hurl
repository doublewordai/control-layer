# =============================================================================
# Endpoints Permissions Test Suite
# =============================================================================
# Tests all CRUD operations on /admin/api/v1/endpoints
#
# Permission model:
# - StandardUser: CANNOT interact with endpoints in any way
#   - Cannot create, read, update, validate, or delete endpoints
#   - Cannot list or view endpoint information
# - PlatformManager: Full access to all endpoint operations
#   - Can create, read, update, validate, and delete endpoints
#   - Can view all endpoints and their configurations
# =============================================================================

# -----------------------------------------------------------------------------
# Setup: Get user IDs
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

# =============================================================================
# Endpoint Creation Permissions (POST /endpoints)
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser CANNOT create endpoints
# -----------------------------------------------------------------------------

# User1 tries to create an endpoint - should be forbidden
POST http://localhost:3001/admin/api/v1/endpoints
[Cookies]
dwctl_session: {{user_jwt}}
{
  "name": "test-endpoint-user1",
  "url": "http://mock-llm-server:3000",
  "sync": false
}
HTTP 403

# User2 tries to create an endpoint - should be forbidden
POST http://localhost:3001/admin/api/v1/endpoints
[Cookies]
dwctl_session: {{user2_jwt}}
{
  "name": "test-endpoint-user2",
  "url": "http://mock-llm-server:3000",
  "sync": false
}
HTTP 403

# -----------------------------------------------------------------------------
# PlatformManager CAN create endpoints
# -----------------------------------------------------------------------------

# Admin creates endpoint 1 (no sync)
POST http://localhost:3001/admin/api/v1/endpoints
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "name": "test-endpoint-1",
  "description": "Test endpoint 1 for permissions testing",
  "url": "http://mock-llm-server:3000",
  "sync": false
}
HTTP 201
[Asserts]
jsonpath "$.id" exists
jsonpath "$.name" == "test-endpoint-1"
jsonpath "$.url" == "http://mock-llm-server:3000/"
jsonpath "$.created_by" == "{{admin_user_id}}"
[Captures]
endpoint1_id: jsonpath "$.id"

# Admin creates endpoint 2 (no sync)
POST http://localhost:3001/admin/api/v1/endpoints
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "name": "test-endpoint-2",
  "description": "Test endpoint 2 for permissions testing",
  "url": "http://mock-llm-server:3000",
  "sync": false
}
HTTP 201
[Asserts]
jsonpath "$.id" exists
jsonpath "$.name" == "test-endpoint-2"
[Captures]
endpoint2_id: jsonpath "$.id"

# Admin creates endpoint 3 (no sync)
POST http://localhost:3001/admin/api/v1/endpoints
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "name": "test-endpoint-3",
  "description": "Test endpoint 3 for permissions testing",
  "url": "http://mock-llm-server:3000",
  "sync": false
}
HTTP 201
[Asserts]
jsonpath "$.id" exists
jsonpath "$.name" == "test-endpoint-3"
[Captures]
endpoint3_id: jsonpath "$.id"

# =============================================================================
# Endpoint Listing Permissions (GET /endpoints)
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser CANNOT list endpoints
# -----------------------------------------------------------------------------

# User1 tries to list endpoints - should be forbidden
GET http://localhost:3001/admin/api/v1/endpoints
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User2 tries to list endpoints - should be forbidden
GET http://localhost:3001/admin/api/v1/endpoints
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 403

# -----------------------------------------------------------------------------
# PlatformManager CAN list all endpoints
# -----------------------------------------------------------------------------

# Admin lists all endpoints - should see all 3 endpoints
GET http://localhost:3001/admin/api/v1/endpoints
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$" isCollection
jsonpath "$[*].id" contains "{{endpoint1_id}}"
jsonpath "$[*].id" contains "{{endpoint2_id}}"
jsonpath "$[*].id" contains "{{endpoint3_id}}"

# =============================================================================
# Endpoint Retrieval Permissions (GET /endpoints/{id})
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser CANNOT fetch endpoints
# -----------------------------------------------------------------------------

# User1 tries to fetch endpoint1 - should be forbidden
GET http://localhost:3001/admin/api/v1/endpoints/{{endpoint1_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User1 tries to fetch endpoint2 - should be forbidden
GET http://localhost:3001/admin/api/v1/endpoints/{{endpoint2_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User2 tries to fetch any endpoint - should be forbidden
GET http://localhost:3001/admin/api/v1/endpoints/{{endpoint3_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 403

# -----------------------------------------------------------------------------
# PlatformManager CAN fetch any endpoint
# -----------------------------------------------------------------------------

# Admin fetches endpoint1 - should succeed
GET http://localhost:3001/admin/api/v1/endpoints/{{endpoint1_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{endpoint1_id}}"
jsonpath "$.name" == "test-endpoint-1"
jsonpath "$.url" == "http://mock-llm-server:3000/"

# Admin fetches endpoint2 - should succeed
GET http://localhost:3001/admin/api/v1/endpoints/{{endpoint2_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{endpoint2_id}}"
jsonpath "$.name" == "test-endpoint-2"

# Admin fetches endpoint3 - should succeed
GET http://localhost:3001/admin/api/v1/endpoints/{{endpoint3_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{endpoint3_id}}"
jsonpath "$.name" == "test-endpoint-3"

# =============================================================================
# Endpoint Update Permissions (PATCH /endpoints/{id})
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser CANNOT update endpoints
# -----------------------------------------------------------------------------

# User1 tries to update endpoint1 - should be forbidden
PATCH http://localhost:3001/admin/api/v1/endpoints/{{endpoint1_id}}
[Cookies]
dwctl_session: {{user_jwt}}
{
  "description": "User1 tried to update this"
}
HTTP 403

# User1 tries to update endpoint2 - should be forbidden
PATCH http://localhost:3001/admin/api/v1/endpoints/{{endpoint2_id}}
[Cookies]
dwctl_session: {{user_jwt}}
{
  "name": "hacked-endpoint"
}
HTTP 403

# User2 tries to update any endpoint - should be forbidden
PATCH http://localhost:3001/admin/api/v1/endpoints/{{endpoint3_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
{
  "description": "User2 tried to update this"
}
HTTP 403

# -----------------------------------------------------------------------------
# PlatformManager CAN update any endpoint
# -----------------------------------------------------------------------------

# Admin updates endpoint1 - should succeed
PATCH http://localhost:3001/admin/api/v1/endpoints/{{endpoint1_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "description": "Updated by admin"
}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{endpoint1_id}}"
jsonpath "$.description" == "Updated by admin"

# Admin updates endpoint2 name - should succeed
PATCH http://localhost:3001/admin/api/v1/endpoints/{{endpoint2_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "name": "test-endpoint-2-renamed"
}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{endpoint2_id}}"
jsonpath "$.name" == "test-endpoint-2-renamed"

# =============================================================================
# Endpoint Validation Permissions (POST /endpoints/validate)
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser CANNOT validate endpoints
# -----------------------------------------------------------------------------

# User1 tries to validate a new endpoint - should be forbidden
POST http://localhost:3001/admin/api/v1/endpoints/validate
[Cookies]
dwctl_session: {{user_jwt}}
{
  "type": "new",
  "url": "http://mock-llm-server:3000"
}
HTTP 403

# User1 tries to validate an existing endpoint - should be forbidden
POST http://localhost:3001/admin/api/v1/endpoints/validate
[Cookies]
dwctl_session: {{user_jwt}}
{
  "type": "existing",
  "endpoint_id": "{{endpoint1_id}}"
}
HTTP 403

# User2 tries to validate any endpoint - should be forbidden
POST http://localhost:3001/admin/api/v1/endpoints/validate
[Cookies]
dwctl_session: {{user2_jwt}}
{
  "type": "new",
  "url": "http://mock-llm-server:3000"
}
HTTP 403

# -----------------------------------------------------------------------------
# PlatformManager CAN validate endpoints
# -----------------------------------------------------------------------------

# Admin validates a new endpoint - should succeed
POST http://localhost:3001/admin/api/v1/endpoints/validate
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "type": "new",
  "url": "http://mock-llm-server:3000"
}
HTTP 200
[Asserts]
jsonpath "$.status" == "success"
jsonpath "$.models" exists

# Admin validates an existing endpoint - should succeed
POST http://localhost:3001/admin/api/v1/endpoints/validate
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "type": "existing",
  "endpoint_id": "{{endpoint1_id}}"
}
HTTP 200
[Asserts]
jsonpath "$.status" == "success"
jsonpath "$.models" exists

# =============================================================================
# Endpoint Deletion Permissions (DELETE /endpoints/{id})
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser CANNOT delete endpoints
# -----------------------------------------------------------------------------

# User1 tries to delete endpoint1 - should be forbidden
DELETE http://localhost:3001/admin/api/v1/endpoints/{{endpoint1_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User1 tries to delete endpoint2 - should be forbidden
DELETE http://localhost:3001/admin/api/v1/endpoints/{{endpoint2_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User2 tries to delete any endpoint - should be forbidden
DELETE http://localhost:3001/admin/api/v1/endpoints/{{endpoint3_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 403

# -----------------------------------------------------------------------------
# PlatformManager CAN delete endpoints
# -----------------------------------------------------------------------------

# Admin deletes endpoint3 - should succeed
DELETE http://localhost:3001/admin/api/v1/endpoints/{{endpoint3_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# Verify endpoint3 is deleted - should get 404
GET http://localhost:3001/admin/api/v1/endpoints/{{endpoint3_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 404

# Admin deletes endpoint2 - should succeed
DELETE http://localhost:3001/admin/api/v1/endpoints/{{endpoint2_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# Admin deletes endpoint1 - should succeed
DELETE http://localhost:3001/admin/api/v1/endpoints/{{endpoint1_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# =============================================================================
# Cleanup Complete
# =============================================================================